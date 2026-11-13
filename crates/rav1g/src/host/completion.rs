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
use std::thread::JoinHandle;

use windows::Win32::Graphics::Direct3D12::ID3D12Fence;
use windows::core::Interface;

use super::create::{rav1g_frame_buffer_acquire, rav1g_release_fb};
use super::frame::{commit_command_buffer, prepare_command_buffer, release_unsubmitted_frame};
use super::output::{rav1g_copy_output, rav1g_output_image};
use super::perf::{av1_perf_ticks, perf_add, perf_callback, perf_collect, perf_now, ticks_to_ms};
use super::stage_dump::{set_dump_info, stage_dump_deliver};
use super::thread::{
    cond_signal, mutex_lock, mutex_unlock, rav1g_mt_queue_get, rav1g_mt_queue_is_empty, rav1g_mt_queue_push,
};
use super::types::{Av1Core, GpuWorkItem, av1_frame_thread_data};
use crate::gpu::Event;
use crate::sys;
use crate::sys::parser::{AV1Decoder, HwOutputImage, aom_image_t, av1_decoded_frame_buffer_t};

unsafe extern "C" {
    /// `av1/decoder/obu.c`: the frame is the one of its temporal unit that is output.
    fn av1_frame_is_output(pbi: *const AV1Decoder) -> c_int;
}

/// Completion thread, -asyncclear: the frame that used `td` has completed on the GPU, so nothing reads
/// the slot's grids any more, and the slot is not in the pool, so the decode thread cannot have it. Clears
/// what the frame cleared at its setup; the next frame of the same size then starts on a slot that is
/// already clean. Returns the bytes cleared.
///
/// # Safety
/// `td` is the slot of a frame that has completed and is not back in its pool.
unsafe fn async_clear_slot(td: &mut av1_frame_thread_data) -> usize {
    let mut bytes = 0;
    if td.clear_pending != 0 && td.async_clear != 0 {
        // SAFETY: the two grids of the slot, of at least the sizes the frame's setup cleared.
        unsafe {
            (*td.mode_info_grid).host_ptr.cast::<u8>().write_bytes(0, td.clear_mi_bytes);
            // The luma grid and the chroma grid behind it, as one range (the two clears of the frame setup).
            if td.clear_iter_bytes != 0 {
                (*td.gen_intra_inter_grid).host_ptr.cast::<u8>().write_bytes(0xFF, td.clear_iter_bytes);
            }
        }
        td.clean_mi_bytes = td.clear_mi_bytes;
        td.clean_iter_bytes = td.clear_iter_bytes;
        bytes = td.clear_mi_bytes + td.clear_iter_bytes;
    }
    td.clear_pending = 0;
    bytes
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
        if rav1g_mt_queue_is_empty(&raw mut (*dec).output_queue) != 0 {
            return 1;
        }
        let hw_img = rav1g_mt_queue_get(&raw mut (*dec).output_queue).cast::<HwOutputImage>();
        copy_to_img(dec, &mut *hw_img, &mut *dst);
        rav1g_mt_queue_push(&raw mut (*dec).image_pool, hw_img.cast());
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
            let item = rav1g_mt_queue_get(&raw mut (*dec).gpu_waiting_queue).cast::<GpuWorkItem>();
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
                mutex_lock(&raw mut (*dec).fb_pool_mutex);
                let mut signal = 0;
                if !td.back_buffer0.is_null() {
                    signal |= rav1g_release_fb(dec, td.back_buffer0);
                }
                signal |= rav1g_release_fb(dec, td.dst_frame_buffer);
                for r in 0..7 {
                    if !td.refs[r].is_null() {
                        signal |= rav1g_release_fb(dec, td.refs[r]);
                    }
                    td.refs[r] = std::ptr::null_mut();
                }
                td.dst_frame_buffer = std::ptr::null_mut();
                td.back_buffer0 = std::ptr::null_mut();
                if signal != 0 {
                    cond_signal(&raw mut (*dec).fb_pool_empty_cond);
                }
                if !td.sec_thread_data.is_null() {
                    rav1g_mt_queue_push(&raw mut (*dec).frame_data_pool, td.sec_thread_data.cast());
                }
                mutex_unlock(&raw mut (*dec).fb_pool_mutex);
                td.sec_thread_data = std::ptr::null_mut();
                // -asyncclear: the slot's grids are cleared here instead of on the decode thread. A second
                // slot went back above as it is: a frame uses only the coefficient and mode info shares of
                // its second slot, not the grids, so that slot is as clean as it was.
                let perf_clear_start = if perf_have_rec { perf_now() } else { 0 };
                let cleared = async_clear_slot(td);
                if perf_clear_start != 0 && cleared != 0 {
                    perf_rec.cpu_ms[sys::AV1_PERF_CPU_ASYNC_CLEAR as usize] =
                        (perf_now() - perf_clear_start) as f64 * ticks_to_ms();
                    perf_rec.async_clear_bytes = cleared as i64;
                }
                // A pipeline slot, or the output context of a show_existing_frame copy.
                let pool = if td.output_only != 0 {
                    &raw mut (*dec).output_data_pool
                } else {
                    &raw mut (*dec).frame_data_pool
                };
                rav1g_mt_queue_push(pool, std::ptr::from_mut(td).cast());
            }

            if let Some(hw_img) = (*item).image.as_mut() {
                if let Some(notify) = notify {
                    let mut img: aom_image_t = std::mem::zeroed();
                    copy_to_img(dec, hw_img, &mut img);
                    rav1g_mt_queue_push(&raw mut (*dec).image_pool, std::ptr::from_mut(hw_img).cast());
                    let perf_notify_start = if perf_have_rec { perf_now() } else { 0 };
                    FRAME_FENCE.set(fence);
                    notify((*dec).image_alloc_priv, (&raw mut img).cast());
                    FRAME_FENCE.set((std::ptr::null_mut(), 0));
                    if perf_notify_start != 0 {
                        perf_rec.cpu_ms[sys::AV1_PERF_CPU_NOTIFY as usize] =
                            (perf_now() - perf_notify_start) as f64 * ticks_to_ms();
                    }
                } else {
                    rav1g_mt_queue_push(&raw mut (*dec).output_queue, std::ptr::from_mut(hw_img).cast());
                }
            }
            if perf_have_rec && let Some((callback, context)) = perf_callback() {
                callback(context, &perf_rec);
            }
            rav1g_mt_queue_push(&raw mut (*dec).gpu_item_pool, item.cast());
        }
    }
}

/// Starts the completion thread when it does not run: with the first frame, and with the first frame
/// behind a drain, which ends it.
///
/// # Safety
/// `dec` is a decoder; decode thread.
pub(crate) unsafe fn start_completion_thread(dec: *mut Av1Core) {
    // SAFETY: the caller's contract. The handle of the thread is kept in the decoder as a pointer to it.
    unsafe {
        if !(*dec).gpu_thread.is_null() {
            return;
        }
        let decoder = DecoderPtr(dec);
        let thread: JoinHandle<()> = std::thread::spawn(move || {
            let decoder = decoder;
            // The decoder joins this thread before it is released (av1_drain_gpu_decoder).
            completion_thread(decoder.0);
        });
        (*dec).gpu_thread = Box::into_raw(Box::new(thread)).cast();
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
    // SAFETY: the caller's contract; the pointer is the box of `start_completion_thread`.
    unsafe {
        if dec.is_null() || (*dec).gpu_thread.is_null() {
            return;
        }
        rav1g_mt_queue_push(&raw mut (*dec).gpu_waiting_queue, std::ptr::null_mut());
        let thread = Box::from_raw((*dec).gpu_thread.cast::<JoinHandle<()>>());
        (*dec).gpu_thread = std::ptr::null_mut();
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
        if av1_frame_is_output(pbi) == 0 {
            return -1;
        }

        let perf_t = av1_perf_ticks();
        if perf_t != 0 {
            let acc = &mut (*dec).perf_cur;
            acc.t_frame_start = perf_t;
            acc.is_sef = 1;
            acc.show_frame = 1;
            acc.intra_iters = -1;
            acc.frame_number = (*buf.hw_buffer).frame_number;
        }
        // Waits when both contexts are in flight: until the copy of an earlier show_existing_frame has
        // completed.
        let td_ptr = rav1g_mt_queue_get(&raw mut (*dec).output_data_pool).cast::<av1_frame_thread_data>();
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
        rav1g_frame_buffer_acquire(dec, td.dst_frame_buffer);
        // From here on the next setup can take the context back if the output image cannot be had.
        (*dec).curr_frame_data = td_ptr;
        // The completion thread ends with a drain; a show_existing_frame may be the first thing after one.
        start_completion_thread(dec);

        prepare_command_buffer(dec);
        if td.dump_active != 0 {
            let info = format!("show_existing_frame film_grain={}", td.do_filmgrain);
            set_dump_info(td, &info);
        }
        rav1g_output_image(dec, cm, fb)
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
            rav1g_copy_output(dec, cm, fb);
        }
        commit_command_buffer(dec);

        let perf_t = av1_perf_ticks();
        let item = rav1g_mt_queue_get(&raw mut (*dec).gpu_item_pool).cast::<GpuWorkItem>();
        perf_add(dec, sys::AV1_PERF_CPU_WAIT_ITEM as usize, perf_t);
        (*item).data = (*dec).curr_frame_data;
        (*item).image = buf.hw_show_image;
        buf.hw_show_image = std::ptr::null_mut();
        (*dec).curr_frame_data = std::ptr::null_mut();
        if let Some(image) = (*item).image.as_mut() {
            image.user_priv = (*pbi).user_priv;
        }
        rav1g_mt_queue_push(&raw mut (*dec).gpu_waiting_queue, item.cast());
    }
}
