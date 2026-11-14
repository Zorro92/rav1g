// SPDX-License-Identifier: BSD-2-Clause
//
// This source code is subject to the terms of the BSD 2 Clause License and the Alliance for Open Media
// Patent License 1.0. If the BSD 2 Clause License was not distributed with this source code in the LICENSE
// file, you can obtain it at www.aomedia.org/license/software. If the Alliance for Open Media Patent
// License 1.0 was not distributed with this source code in the PATENTS file, you can obtain it at
// www.aomedia.org/license/patent.

//! What happens to a frame behind its submission (`av1_core.cpp`): the completion thread waits for the
//! fence of each frame in the order of submission, gives back the frame buffers the frame counted and its
//! pipeline slot, and hands the output image to the application; `show_existing_frame` copies a frame
//! that was decoded earlier into an output image with a command list of its own.

use std::cell::Cell;
use std::ffi::{c_int, c_void};

use windows::Win32::Graphics::Direct3D12::ID3D12Fence;
use windows::core::Interface;

use super::frame::{commit_command_buffer, prepare_command_buffer, release_unsubmitted_frame};
use super::log::log;
use super::output::{copy_output, output_image};
use super::perf::{av1_perf_ticks, perf_add, perf_callback, perf_collect, perf_now, ticks_to_ms};
use super::stage_dump::{set_dump_info, stage_dump_deliver};
use super::thread::spawn_thread;
use super::types::{Av1Core, GpuWorkItem, av1_frame_thread_data};
use crate::gpu::Event;
use crate::sys;
use crate::sys::parser::{AV1Decoder, HwOutputImage, aom_image_t, av1_decoded_frame_buffer_t};

unsafe extern "C" {
    /// `av1/decoder/obu.c`: the frame is the one of its temporal unit that is output.
    fn av1_frame_is_output(pbi: *const AV1Decoder) -> c_int;
}

/// The frame that used `td` has completed on the GPU, so nothing reads the slot's grids any more, and
/// the slot is not in the pool, so the decode thread cannot have it. What the frame cleared at its setup
/// is clean again behind it, so that the next frame of the same size starts on a slot that is: cleared
/// here, by the completion thread, or already cleared by the frame's own command list (`AV1_CLEARS_GPU`,
/// `record_slot_clears`); not with the clears on the decode thread (`AV1_CLEARS_DECODE_THREAD`). Without
/// a switch a frame of a decoder with the parser of phase 2 has cleared nothing, and nothing is cleared
/// here. Returns the bytes cleared here, and the bytes the command list cleared.
///
/// # Safety
/// `td` is the slot of a frame that has completed and is not back in its pool.
unsafe fn clear_slot(td: &mut av1_frame_thread_data) -> (usize, usize) {
    let bytes = td.clear_mi_bytes + td.clear_iter_bytes;
    let mut cleared = (0, 0);
    if td.clear_pending != 0 && td.clears != sys::AV1_CLEARS_DECODE_THREAD as c_int {
        if td.clears != sys::AV1_CLEARS_GPU as c_int {
            // SAFETY: the two grids of the slot, of at least the sizes the frame's setup cleared.
            unsafe {
                (*td.mode_info_grid).host_ptr.cast::<u8>().write_bytes(0, td.clear_mi_bytes);
                // The luma grid and the chroma grid behind it, as one range (the two clears of the frame
                // setup).
                if td.clear_iter_bytes != 0 {
                    (*td.gen_intra_inter_grid).host_ptr.cast::<u8>().write_bytes(0xFF, td.clear_iter_bytes);
                }
            }
            cleared.0 = bytes;
        } else if td.gpu_cleared != 0 {
            cleared.1 = bytes;
        }
        if td.clears != sys::AV1_CLEARS_GPU as c_int || td.gpu_cleared != 0 {
            td.clean_mi_bytes = td.clear_mi_bytes;
            td.clean_iter_bytes = td.clear_iter_bytes;
        }
    }
    td.clear_pending = 0;
    td.gpu_cleared = 0;
    cleared
}

/// `copy_to_img` of the C++: the output image as the application gets it, and the pool entry empty again.
///
/// # Safety
/// `dec` is the decoder and `hw_img` an output image of it that holds an image of the application.
unsafe fn copy_to_img(dec: *const Av1Core, hw_img: &mut HwOutputImage, dst: &mut aom_image_t) {
    dst.bit_depth = if hw_img.hbd != 0 { 10 } else { 8 };
    dst.w = hw_img.y_crop_width as u32;
    dst.h = hw_img.y_crop_height as u32;
    dst.d_w = hw_img.y_crop_width as u32;
    dst.d_h = hw_img.y_crop_height as u32;
    dst.r_w = hw_img.y_crop_width as u32;
    dst.r_h = hw_img.y_crop_height as u32;
    dst.x_chroma_shift = 1;
    dst.y_chroma_shift = 1;
    for plane in 0..3 {
        dst.planes[plane] = hw_img.fb_ptr.wrapping_offset(hw_img.planes[plane].offset as isize);
        dst.stride[plane] = hw_img.planes[plane].stride;
    }
    dst.user_priv = hw_img.user_priv;
    dst.fb2_priv = hw_img.alloc_priv;
    dst.monochrome = hw_img.monochrome;
    if hw_img.monochrome != 0 {
        dst.planes[1] = std::ptr::null_mut();
        dst.planes[2] = std::ptr::null_mut();
    }
    // What the stream says about the colour of the frame, for the application.
    dst.cp = hw_img.color_primaries.into();
    dst.tc = hw_img.transfer_characteristics.into();
    dst.mc = hw_img.matrix_coefficients.into();
    dst.range = hw_img.color_range.into();
    dst.csp = hw_img.chroma_sample_position.into();
    dst.color_description_present = hw_img.color_description_present.into();
    dst.hdr = hw_img.hdr;
    // SAFETY: the caller's contract.
    dst.is_hdr10x3 = unsafe { (*dec).tryhdr10x3 };
    hw_img.fb_ptr = std::ptr::null_mut();
    hw_img.alloc_priv = std::ptr::null_mut();
    hw_img.user_priv = std::ptr::null_mut();
    hw_img.is_valid = 0;
}

/// `get_output_frame` (`dx/av1_core.h`): the next output image, for an application that takes its frames
/// from the decode call and has no notification callback. Returns 1 when there is none.
///
/// # Safety
/// `pbi` is the entropy decoder's state of a decoder; `dst` is valid for writing.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn get_output_frame(pbi: *mut AV1Decoder, dst: *mut aom_image_t) -> c_int {
    // SAFETY: the caller's contract.
    unsafe {
        let dec = (*pbi).gpu_decoder.cast::<Av1Core>();
        let Some(hw_img) = (*dec).output_queue.try_get() else {
            return 1;
        };
        copy_to_img(dec, &mut *hw_img, &mut *dst);
        (*dec).image_pool.push(hw_img);
    }
    0
}

thread_local! {
    /// The fence of the frame the completion thread is handing to the application, and the value the
    /// fence had to reach for it; null outside of that call.
    static FRAME_FENCE: Cell<(*mut c_void, u64)> = const { Cell::new((std::ptr::null_mut(), 0)) };
}

/// For the frame-ready callback of the Rust interface (`crate::decoder`), which the completion thread
/// calls: the fence of the command list that wrote the frame it is called for, and the value of the fence
/// behind that list. The image description of the C interface has no room for them.
pub(crate) fn frame_fence() -> Option<(ID3D12Fence, u64)> {
    let (fence, value) = FRAME_FENCE.get();
    // SAFETY: set by the completion thread around the callback to the fence of a slot of its decoder,
    // which lives as long as the decoder.
    unsafe { ID3D12Fence::from_raw_borrowed(&fence) }.map(|fence| (fence.clone(), value))
}

/// The decoder for its completion thread.
struct DecoderPtr(*mut Av1Core);

// SAFETY: the completion thread uses the decoder as the C++ thread did: the queues and pools under their
// locks, and of a slot only what the decode thread does not touch while the slot is out of its pool. The
// thread is joined before the decoder is released.
unsafe impl Send for DecoderPtr {}

/// The completion thread (`gpu_thread_hook` of the C++): takes the work items in the order the decode
/// thread queued them, until the item that is none (the drain).
///
/// # Safety
/// `dec` is a decoder that outlives the thread.
unsafe fn completion_thread(dec: *mut Av1Core) {
    // SAFETY: the caller's contract, for all of this function.
    unsafe {
        let notify = (*dec).cb_notify_frame_ready.filter(|_| !(*dec).image_alloc_priv.is_null());
        loop {
            let item: *mut GpuWorkItem = (*dec).gpu_waiting_queue.get();
            if item.is_null() {
                if let Some(notify) = notify {
                    notify((*dec).image_alloc_priv, std::ptr::null_mut());
                }
                break;
            }

            let mut perf_rec: sys::av1_perf_frame = std::mem::zeroed();
            let mut perf_have_rec = false;
            let mut fence: (*mut c_void, u64) = (std::ptr::null_mut(), 0);
            if let Some(td) = (*item).data.as_mut() {
                // Of the frame's command list, read while the slot is still this frame's.
                if let Some(slot_fence) = td.command_buffer.fence.as_ref() {
                    fence = (slot_fence.d3d12().as_raw(), u64::from(td.command_buffer.fence_value));
                }
                // Waits for the frame's fence.
                let perf_wait_start = if td.perf.collect != 0 { perf_now() } else { 0 };
                Event::borrowed(&td.command_buffer.event).wait();
                // -dumpstages: the copies of the frame's stages, while the slot is still this frame's.
                if td.dump_count != 0 {
                    stage_dump_deliver(td);
                }
                if perf_wait_start != 0 {
                    // The frame's timestamps are read now: its fence is done and the slot has not gone back
                    // to the pool yet.
                    perf_collect(dec, td, perf_wait_start, &mut perf_rec);
                    perf_have_rec = true;
                }
                {
                    // The frame's buffers and its second slot go back under the lock of the buffer pool.
                    let mut pool = (*dec).fb_pool.lock();
                    let mut returned = false;
                    if !td.back_buffer0.is_null() {
                        returned |= pool.release(td.back_buffer0);
                    }
                    returned |= pool.release(td.dst_frame_buffer);
                    for r in 0..7 {
                        if !td.refs[r].is_null() {
                            returned |= pool.release(td.refs[r]);
                        }
                        td.refs[r] = std::ptr::null_mut();
                    }
                    td.dst_frame_buffer = std::ptr::null_mut();
                    td.back_buffer0 = std::ptr::null_mut();
                    if returned {
                        (*dec).fb_pool.signal();
                    }
                    if !td.sec_thread_data.is_null() {
                        (*dec).frame_data_pool.push(td.sec_thread_data);
                    }
                }
                td.sec_thread_data = std::ptr::null_mut();
                // What the frame took of the pools is free for the frames in parse. Its coefficients
                // are zero again: the transform stage clears what it reads.
                if let (0, Some(pools)) = (td.output_only, (*dec).pools.as_deref()) {
                    pools.release(td.slot_index as usize, |_, _, _| {});
                }
                // What the frame cleared of the slot's grids is clean again before the slot goes back
                // (`clear_slot`). A second slot went back above as it is: a frame uses only the coefficient
                // and mode info shares of its second slot, not the grids, so that slot is as clean as it
                // was.
                let perf_clear_start = if perf_have_rec { perf_now() } else { 0 };
                let (cleared, gpu_cleared) = clear_slot(td);
                if perf_clear_start != 0 && cleared != 0 {
                    perf_rec.cpu_ms[sys::AV1_PERF_CPU_ASYNC_CLEAR as usize] =
                        (perf_now() - perf_clear_start) as f64 * ticks_to_ms();
                    perf_rec.async_clear_bytes = cleared as i64;
                }
                if perf_have_rec {
                    perf_rec.gpu_clear_bytes = gpu_cleared as i64;
                }
                // A pipeline slot, or the output context of a show_existing_frame copy.
                let pool = if td.output_only != 0 { &(*dec).output_data_pool } else { &(*dec).frame_data_pool };
                pool.push(std::ptr::from_mut(td));
            }

            if let Some(hw_img) = (*item).image.as_mut() {
                if let Some(notify) = notify {
                    let mut img: aom_image_t = std::mem::zeroed();
                    copy_to_img(dec, hw_img, &mut img);
                    (*dec).image_pool.push(std::ptr::from_mut(hw_img));
                    let perf_notify_start = if perf_have_rec { perf_now() } else { 0 };
                    FRAME_FENCE.set(fence);
                    notify((*dec).image_alloc_priv, (&raw mut img).cast());
                    FRAME_FENCE.set((std::ptr::null_mut(), 0));
                    if perf_notify_start != 0 {
                        perf_rec.cpu_ms[sys::AV1_PERF_CPU_NOTIFY as usize] =
                            (perf_now() - perf_notify_start) as f64 * ticks_to_ms();
                    }
                } else {
                    (*dec).output_queue.push(std::ptr::from_mut(hw_img));
                }
            }
            if perf_have_rec && let Some((callback, context)) = perf_callback() {
                callback(context, &perf_rec);
            }
            (*dec).gpu_item_pool.push(item);
        }
    }
}

/// Starts the completion thread when it does not run: with the first frame, and with the first frame
/// behind a drain, which ends it.
///
/// # Safety
/// `dec` is a decoder; decode thread.
pub(crate) unsafe fn start_completion_thread(dec: *mut Av1Core) -> bool {
    // SAFETY: the caller's contract. The decoder keeps the handle of the thread.
    unsafe {
        if (*dec).gpu_thread.is_some() {
            return true;
        }
        let decoder = DecoderPtr(dec);
        let thread = spawn_thread(move || {
            let decoder = decoder;
            // The decoder joins this thread before it is released (av1_drain_gpu_decoder).
            completion_thread(decoder.0);
        });
        match thread {
            Ok(thread) => {
                (*dec).gpu_thread = Some(thread);
                true
            }
            Err(error) => {
                log(true, &format!("MEMORY FAILED GPU completion thread: {error}"));
                false
            }
        }
    }
}

/// `av1_drain_gpu_decoder` (`dx/av1_core.h`): waits until every frame that was submitted has completed
/// and its image went to the application: the completion thread gets the item that ends it, and is
/// joined.
///
/// # Safety
/// `dec` is null or a decoder; decode thread.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn av1_drain_gpu_decoder(dec: *mut Av1Core) {
    // SAFETY: the caller's contract.
    unsafe {
        if dec.is_null() {
            return;
        }
        // With more than one frame in parse, frames may still be in parse or wait for their submission:
        // they are submitted first.
        super::rav1d_host::drain_frames(dec);
        let Some(thread) = (*dec).gpu_thread.take() else {
            return;
        };
        (*dec).gpu_waiting_queue.push(std::ptr::null_mut());
        let _ = thread.join();
    }
}

/// `av1_decode_sef` (`dx/av1_core.h`), its first part.
///
/// show_existing_frame: the frame in `cm->cur_frame` (the frame to show; the header reader has put it
/// there, set `cm->show_frame` and loaded the frame's film grain parameters into
/// `cm->film_grain_params`, the load_grain_params of the specification) is output now. The frame was
/// decoded earlier; it took no output image then unless it was shown then, and that image went to the
/// application. So the output is made here: an output image from the application, and a command list of
/// its own that copies the frame buffer into it, with film grain.
///
/// Order on the GPU: the command list of the frame's decode was submitted by this thread before this
/// header was read, and the lists run on the one queue in the order of their submission, so the copy
/// reads the finished frame. Nothing writes a frame buffer after the frame's own list.
/// Lifetime: the copy holds a reference to the frame buffer (taken while the reference map still holds
/// the frame) until the completion thread has seen the fence of the copy, so the buffer cannot go back to
/// the pool and be written by a later frame before the copy has run, whatever the stream does to the
/// reference map in between.
/// Output order: the work item is queued behind those of the frames before it, and the completion thread
/// hands the images over in that order.
///
/// The copy takes an output context, not a pipeline slot: a slot would be one frame less in flight for
/// as long as the copy waits behind the frames in front of it on the queue.
///
/// Returns -1 when there is nothing to output, else the state of the output image: 1 `fb` is the image,
/// 2 the application gave none (the caller raises the decoder's error), 0 no image is wanted.
///
/// # Safety
/// `pbi` is the entropy decoder's state behind the header of a show_existing_frame; `fb` is valid for
/// writing; decode thread.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rav1g_decode_sef_begin(pbi: *mut AV1Decoder, fb: *mut av1_decoded_frame_buffer_t) -> c_int {
    // SAFETY: the caller's contract.
    unsafe { decode_sef_begin(pbi, fb, None) }
}

/// [`rav1g_decode_sef_begin`]. `output`: whether the frame is the one of its temporal unit that is
/// output, for a caller that asked when the header was read (the question looks through the data of
/// the decode call, which a copy that is recorded later no longer has); `None` asks here.
///
/// # Safety
/// As for [`rav1g_decode_sef_begin`]; the thread that records the frames.
pub(crate) unsafe fn decode_sef_begin(
    pbi: *mut AV1Decoder,
    fb: *mut av1_decoded_frame_buffer_t,
    output: Option<bool>,
) -> c_int {
    // SAFETY: the caller's contract.
    unsafe {
        let dec = (*pbi).gpu_decoder.cast::<Av1Core>();
        let cm = &raw mut (*pbi).common;
        let buf = &mut (*(*cm).cur_frame).buf;
        // A frame before this one that failed after its setup.
        release_unsubmitted_frame(dec);
        buf.hw_show_image = std::ptr::null_mut();
        // Nothing to show: no frame was ever decoded into this entry.
        if buf.hw_buffer.is_null() {
            return -1;
        }
        // Not the frame of its temporal unit that is output: a frame of a higher spatial layer follows.
        if !output.unwrap_or_else(|| av1_frame_is_output(pbi) != 0) {
            return -1;
        }

        let perf_t = av1_perf_ticks();
        if perf_t != 0 {
            let acc = &mut *super::perf::frame_acc(dec);
            acc.t_frame_start = perf_t;
            acc.is_sef = 1;
            acc.show_frame = 1;
            acc.intra_iters = -1;
            acc.frame_number = (*buf.hw_buffer).frame_number;
        }
        // Waits when both contexts are in flight: until the copy of an earlier show_existing_frame has
        // completed.
        let td_ptr: *mut av1_frame_thread_data = (*dec).output_data_pool.get();
        perf_add(dec, sys::AV1_PERF_CPU_WAIT_SLOT as usize, perf_t);
        let td = &mut *td_ptr;
        td.sec_thread_data = std::ptr::null_mut();
        td.back_buffer0 = std::ptr::null_mut();
        td.refs = [std::ptr::null_mut(); 8];
        td.clear_pending = 0;
        td.frame_number = (*buf.hw_buffer).frame_number;
        td.dst_frame_buffer = buf.hw_buffer;
        td.frame_buffer = buf.hw_buffer;
        td.is_hbd = c_int::from(buf.bit_depth > 8);
        td.bitdepth = buf.bit_depth as c_int;
        let lib = (*dec).shader_lib;
        td.shaders = if td.is_hbd != 0 { &raw mut (*lib).shaders_hbd } else { &raw mut (*lib).shaders_8bit };
        td.do_cdef = 0;
        td.do_loop_rest = 0;
        td.do_superres = 0;
        td.do_filmgrain = (*cm).film_grain_params.apply_grain;
        td.output_frame = 1;
        (*dec).fb_pool.acquire(td.dst_frame_buffer);
        // From here on the next setup can take the context back if the output image cannot be had.
        (*dec).curr_frame_data = td_ptr;
        // The completion thread ends with a drain; a show_existing_frame may be the first thing after one.
        if !start_completion_thread(dec) {
            return 2; // the C entry raises the memory error, after Rust has returned
        }

        prepare_command_buffer(dec);
        if td.dump_active != 0 {
            let info = format!("show_existing_frame film_grain={}", td.do_filmgrain);
            set_dump_info(td, &info);
        }
        output_image(dec, cm, fb)
    }
}

/// `av1_decode_sef`, its second part: the copy into the output image is recorded and submitted, and the
/// image goes to the completion thread with the output context.
///
/// # Safety
/// As for [`rav1g_decode_sef_begin`], after it returned a state that is not -1.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rav1g_decode_sef_submit(
    pbi: *mut AV1Decoder,
    image_state: c_int,
    fb: *const av1_decoded_frame_buffer_t,
) {
    // SAFETY: the caller's contract.
    unsafe {
        let dec = (*pbi).gpu_decoder.cast::<Av1Core>();
        let cm = &raw mut (*pbi).common;
        let buf = &mut (*(*cm).cur_frame).buf;
        if image_state != 0 {
            copy_output(dec, cm, fb);
        }
        commit_command_buffer(dec);

        let perf_t = av1_perf_ticks();
        let item: *mut GpuWorkItem = (*dec).gpu_item_pool.get();
        perf_add(dec, sys::AV1_PERF_CPU_WAIT_ITEM as usize, perf_t);
        (*item).data = (*dec).curr_frame_data;
        (*item).image = buf.hw_show_image;
        buf.hw_show_image = std::ptr::null_mut();
        (*dec).curr_frame_data = std::ptr::null_mut();
        if let Some(image) = (*item).image.as_mut() {
            image.user_priv = (*pbi).user_priv;
        }
        (*dec).gpu_waiting_queue.push(item);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::thread::spawn_test;

    /// The system gives no thread for the completion thread: the frame setup is told so (and raises the
    /// decoder's error); the decoder has no thread to join.
    #[test]
    fn no_completion_thread_is_an_error_not_a_panic() {
        let mut dec = Av1Core::new().expect("memory for a decoder");
        let dec = &raw mut *dec;
        // SAFETY: an empty decoder.
        unsafe {
            spawn_test::FAIL.set(true);
            let started = start_completion_thread(dec);
            spawn_test::FAIL.set(false);
            assert!(!started);
            assert!((*dec).gpu_thread.is_none());
            // The drain of a decoder without a completion thread returns at once.
            av1_drain_gpu_decoder(dec);
        }
    }
}
