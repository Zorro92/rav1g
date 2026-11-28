// SPDX-License-Identifier: BSD-2-Clause
//
// This source code is subject to the terms of the BSD 2 Clause License and the Alliance for Open Media
// Patent License 1.0. If the BSD 2 Clause License was not distributed with this source code in the LICENSE
// file, you can obtain it at www.aomedia.org/license/software. If the Alliance for Open Media Patent
// License 1.0 was not distributed with this source code in the PATENTS file, you can obtain it at
// www.aomedia.org/license/patent.

//! A frame on the decode thread (`av1_core.cpp`): the functions the entropy decoder calls
//! (`dx/av1_core.h`) from the header of a frame to its submission. A frame takes a pipeline slot and its
//! frame buffers at its setup, the tile threads get their part of the slot's buffers, and when the tiles
//! are decoded the stages are recorded into the decoder's command list, which is submitted with a fence
//! for the completion thread.
//!
//! The functions that can fail with the decoder's error (`aom_internal_error`, which leaves the decode
//! call with a `longjmp`) are in parts here: the C of `crates/rav1g-sys/host_entry.c` calls the parts and
//! raises the error between them, so that the jump never passes a Rust function.

use std::ffi::{c_int, c_void};

use windows::Win32::Graphics::Direct3D12::{ID3D12CommandQueue, ID3D12Device};

use super::blocks::{WAVE_COLUMN_RESET, WaveColumn, WaveEdges};
use super::completion::start_completion_thread;
use super::create::to_mib;
use super::log::log;
use super::loopfilter::loopfilter_gpu;
use super::memory::poison_constants;
use super::output::{copy_output, frame_unreferenced, output_image, prepare_fused_output};
use super::perf::{
    inter_fast_mode, perf_add, perf_add_memset, perf_begin_command_buffer, perf_detail_mode, perf_lap, perf_now,
    perf_resolve,
};
use super::pool::{CHUNK_ITEMS, COEF_CHUNK, Kind, Take};
use super::postfilter::cdef_looprestoration;
use super::prediction::{inter_ext_borders, prediction_gen_blocks, prediction_run_all};
use super::stage_buffers::MEMORY_GUARD_MARGIN_BUFFER;
use super::stage_dump::{stage_dump_begin, stage_dump_frame_info};
use super::transform::{idct_front, idct_run};
use super::types::{
    Av1Core, DeviceUpload, EntropyThreadCount, GpuBufferObject, GpuWorkItem, InterFastMapSize, Run, TilePieces,
    av1_frame_thread_data,
};
use crate::gpu::d3d12::{com_ref, wrapped_ref};
use crate::gpu::{CommandAllocator, CommandList, Event, Fence, Queue};
use crate::sys;
use crate::sys::{
    ALTREF_FRAME, AV1_COMMON, AV1Decoder, HwFrameBuffer, HwOutputImage, INVALID_IDX, LAST_FRAME, MB_MODE_INFO,
    REF_NO_SCALE, RESTORE_NONE, RefCntBuffer, TileInfo, YV12_BUFFER_CONFIG, av1_decoded_frame_buffer_t, av1_tile_data,
    scale_factors,
};

/// `REF_INVALID_SCALE` of `av1/common/scale.h`.
const REF_INVALID_SCALE: c_int = -1;
/// `TRANSLATION` of `av1/common/mv.h`: the last global motion type that is not a warp.
const TRANSLATION: c_int = 1;
/// `YV12_FLAG_HIGHBITDEPTH` of `aom_scale/yv12config.h`.
const YV12_FLAG_HIGHBITDEPTH: c_int = 8;

/// `av1_shader_variant` (`dx/av1_perf.h`): "uavsync" for the build whose shaders have their group syncs
/// as syncs of all memory (the feature `uavsync`), "shader-pick" for the build with alternate bytecodes,
/// else an empty string.
#[unsafe(no_mangle)]
pub extern "C" fn av1_shader_variant() -> *const std::ffi::c_char {
    if cfg!(feature = "uavsync") {
        c"uavsync".as_ptr()
    } else if cfg!(feature = "shader-pick") {
        c"shader-pick".as_ptr()
    } else {
        c"".as_ptr()
    }
}

/// The merged front of a frame's command list: the work in front of the transforms, by what it reads,
/// in two dependent steps instead of five. Nothing of it reads what the frame's reconstruction writes:
///
/// 1. The mode info grid build (`mi_grid_build`, with `-parseopt gpugrid`, the default), as before.
/// 2. Everything that reads the frame's records and the grid, behind one barrier: the residual clear and
///    the sort of the transform blocks (they read the transform records only), the CDEF lists
///    (`cdef_lists_v2`, AV1 7.15: `cdef_idx` and `skip`), the loop filter's edge lists
///    (`gen_lf_blocks_*`, AV1 7.14.2: levels and transform edges from the mode info) and
///    gen_pred_blocks.
///
/// Before (`-frontmergelegacy`, retired after its console A/B) these were five steps: the grid; gen_pred_blocks;
/// the clear with the sort; and later the edge lists in front of the loop filter and the CDEF lists in front of
/// CDEF. Each dependent step costs about 18 us fixed plus the latency of its slowest group on the console
/// (AGENTS.md, "Dependent dispatches"). gen_pred_blocks and the list passes are latency-bound walks of the mode
/// info that leave the GPU mostly idle; the clear (25 MB at 4K 10-bit) is bound by memory bandwidth: in one step
/// they overlap. The clear goes with the longer of the two steps (the lists and gen_pred_blocks, ~0.25 ms on a 4K
/// frame, ~0.6 ms with the loop filter's lists) rather than with the grid build. Sized for the GPU slice Bale's
/// D3D11 stage shares, at no cost in memory or bytes moved: the same dispatches and buffers, fewer barriers. The
/// decoded frames are the same: only the order of independent work changes.
///
/// gen_pred_blocks moves the tile with the most intra iterations to the front of the frame's tiles
/// (and its pool pieces with it), after the sort has numbered the transform blocks in the tiles' earlier
/// order: `idct_run` takes each type's start as the smallest over the tiles, not from the first tile.
///
/// Timing (`-gputime`): `gen_blocks` is the whole front (grid, clear, sort, lists); `residual_clear`,
/// `idct_sort`, `lf_gen` and `cdef_lists` have no mark of their own.
///
/// # Safety
/// `pbi` is the entropy decoder with the frame of `dec`'s current command list, whose tiles are parsed.
unsafe fn record_front(pbi: *mut AV1Decoder, dec: *mut Av1Core) -> Option<super::loopfilter::LfLists> {
    // SAFETY: the caller's contract.
    unsafe {
        let cm = &raw mut (*pbi).common;
        // Step 1: -parseopt gpugrid: the mode info grid of the frame, built by the GPU from the records
        // in front of every stage that reads it (and of the stage dump of the mode info, `gpu_grid.rs`).
        super::gpu_grid::record(dec, cm);
        // Step 2. The CDEF lists first: their transitions are barriers, so they go in front of every
        // dispatch of the step.
        let cdef_lists = super::postfilter::cdef_lists_front(dec, &*cm);
        idct_front(dec);
        let lf_lists = super::loopfilter::loopfilter_lists(dec, cm, &raw mut (*pbi).mb);
        // Its barrier and mark close the step, with the barriers of the others right behind.
        prediction_gen_blocks(pbi, dec);
        super::transform::idct_front_barrier(dec);
        if lf_lists.is_some() {
            super::loopfilter::loopfilter_lists_barrier(dec);
        }
        if cdef_lists {
            super::postfilter::cdef_lists_front_end(dec);
        }
        lf_lists
    }
}

// ---------------------------------------------------------------------------------------------------------
// The command buffer of a frame.
// ---------------------------------------------------------------------------------------------------------

/// `av1_prepare_command_buffer` of the C++: the command list starts over on the allocator of the frame's
/// slot, and so do the slot's constants, its timestamps and its stage dumps.
///
/// # Safety
/// `dec` is the decoder with the slot (or output context) of the frame in `curr_frame_data`; the thread
/// that records the frames.
pub(crate) unsafe fn prepare_command_buffer(dec: *mut Av1Core) {
    // SAFETY: the caller's contract.
    unsafe {
        let td = (*dec).curr_frame_data;
        stage_dump_begin(&mut *td, (*td).output_only);

        let allocator = wrapped_ref::<_, CommandAllocator>(&(*td).command_buffer.allocator);
        let allocator = allocator.expect("a slot has its command allocator");
        let list = wrapped_ref::<_, CommandList>(&(*dec).compute.command_list).expect("the decoder has its list");
        allocator.reset();
        list.reset(allocator);
        (*td).command_buffer.cb_ptr = 0;

        perf_begin_command_buffer(dec, td);
    }
}

/// `av1_commit_command_buffer` of the C++: closes the command list, submits it, and sets the slot's fence
/// behind it with the event the completion thread waits on.
///
/// # Safety
/// As for [`prepare_command_buffer`], after the frame was recorded.
pub(crate) unsafe fn commit_command_buffer(dec: *mut Av1Core) {
    // SAFETY: the caller's contract.
    unsafe {
        let td = (*dec).curr_frame_data;
        let collect = (*td).perf.collect != 0;
        let mut perf_submit_start = 0;
        if collect {
            // The timestamps of the frame are resolved as the last command of its list.
            perf_resolve(dec, td);
            perf_submit_start = perf_now();
        }

        let list = wrapped_ref::<_, CommandList>(&(*dec).compute.command_list).expect("the decoder has its list");
        let queue = com_ref::<_, ID3D12CommandQueue>(&(*dec).compute.queue).expect("the decoder has its queue");
        let cb = &mut (*td).command_buffer;
        let fence = wrapped_ref::<_, Fence>(&cb.fence).expect("a slot has its fence");
        list.close_quietly();
        cb.fence_value += 1;
        let queue = Queue::new(queue);
        queue.execute(list);
        // The output fence first: when the slot's fence, which the completion thread waits for, has its
        // value, the output fence has the value of this list.
        if let Some(output_fence) = wrapped_ref::<_, Fence>(&(*dec).output_fence) {
            (*dec).output_fence_value += 1;
            cb.output_fence_value = (*dec).output_fence_value;
            queue.signal(output_fence, cb.output_fence_value);
        }
        queue.signal(fence, u64::from(cb.fence_value));
        fence.set_event(u64::from(cb.fence_value), Event::borrowed(&cb.event));

        if collect {
            // The snapshot of the CPU side of this frame. The completion thread cannot see this slot until
            // the caller queues its work item, so plain writes are safe here.
            let acc = &mut *super::perf::frame_acc(dec);
            let now = perf_now();
            let waits =
                acc.ticks[sys::AV1_PERF_CPU_WAIT_OUTPUT as usize] + acc.ticks[sys::AV1_PERF_CPU_WAIT_IMAGE as usize];
            let record = perf_submit_start - acc.t_record_start - waits;
            acc.ticks[sys::AV1_PERF_CPU_RECORD as usize] = record.max(0);
            acc.ticks[sys::AV1_PERF_CPU_SUBMIT as usize] = now - perf_submit_start;
            acc.ticks[sys::AV1_PERF_CPU_FRAME as usize] = now - acc.t_frame_start;
            (*td).perf.acc = *acc;
            (*td).perf.t_submit = now;
            *acc = std::mem::zeroed();
        }
    }
}

// ---------------------------------------------------------------------------------------------------------
// What the parser's host asks for outside a frame's setup.
// ---------------------------------------------------------------------------------------------------------

/// `av1_sequence_fits_decoder` (`dx/av1_core.h`): whether the frames of a sequence fit the buffers the
/// decoder was created with: its largest frame, rounded up to 128 as the buffers are, and its bit depth
/// (frame buffers of one byte a sample hold 8-bit frames only; those of two bytes hold both). The sizes
/// the decoder has go to `dec_*` for the message.
///
/// # Safety
/// `dec` is a decoder; the three out pointers are null or valid for writing.
pub(crate) unsafe extern "C" fn av1_sequence_fits_decoder(
    dec: *mut Av1Core,
    max_frame_width: c_int,
    max_frame_height: c_int,
    bit_depth: c_int,
    dec_width: *mut c_int,
    dec_height: *mut c_int,
    dec_bitdepth: *mut c_int,
) -> c_int {
    // SAFETY: the caller's contract.
    unsafe {
        let dec = &*dec;
        if !dec_width.is_null() {
            *dec_width = dec.max_width;
        }
        if !dec_height.is_null() {
            *dec_height = dec.max_height;
        }
        if !dec_bitdepth.is_null() {
            *dec_bitdepth = dec.max_bitdepth;
        }
        c_int::from(
            ((max_frame_width + 127) & !127) <= dec.max_width
                && ((max_frame_height + 127) & !127) <= dec.max_height
                && (bit_depth <= 8 || dec.max_bitdepth > 8),
        )
    }
}

/// Whether a frame of this size fits the buffers the decoder was created with: its bytes, with and
/// without superres, in a frame buffer of `size` bytes, and its width and height in the tables, lists
/// and grids, which are sized for the width and the height of the creation. A frame that is wider or
/// higher than that is refused even when its bytes would fit a frame buffer.
fn frame_fits(dec: &Av1Core, size: usize, width: c_int, height: c_int, upscaled_width: c_int, hbd: c_int) -> bool {
    let h_border = 16;
    let aligned_width = (width + 127) & !127;
    let aligned_height = (height + 127) & !127;
    let y_stride = aligned_width + 2 * h_border;
    let y_height = aligned_height;
    let y_size = y_height * y_stride;
    let uv_stride = (aligned_width >> 1) + 2 * h_border;
    let uv_size = (y_height >> 1) * uv_stride;
    let fb_size = (y_size + 2 * uv_size) * (1 + hbd);

    let superres_w = (upscaled_width + 127) & !127;
    let superres_size =
        (y_height * (superres_w + 2 * h_border) + 2 * (y_height >> 1) * ((superres_w >> 1) + 2 * h_border)) * (1 + hbd);
    !(size < fb_size as usize
        || size < superres_size as usize
        || aligned_width > dec.max_width
        || superres_w > dec.max_width
        || aligned_height > dec.max_height
        || (width != upscaled_width && dec.enable_superres == 0))
}

/// The description of a frame of this size, but for the frame buffer it is decoded into
/// (`hw_buffer`, which is left alone).
fn describe_frame(ybf: &mut YV12_BUFFER_CONFIG, width: c_int, height: c_int, hbd: c_int) {
    let h_border = 16;
    let aligned_width = (width + 127) & !127;
    let aligned_height = (height + 127) & !127;
    let y_stride = aligned_width + 2 * h_border;
    let y_size = aligned_height * y_stride;
    let uv_stride = (aligned_width >> 1) + 2 * h_border;
    let uv_size = (aligned_height >> 1) * uv_stride;
    let fb_size = (y_size + 2 * uv_size) * (1 + hbd);

    ybf.buffer_alloc = std::ptr::null_mut();
    ybf.buffer_alloc_sz = 0;
    ybf.crop_widths[0] = width; // y_crop_width
    ybf.crop_heights[0] = height; // y_crop_height
    let y_width = (width + 7) & !7;
    let y_height_visible = (height + 7) & !7;
    ybf.widths[0] = y_width;
    ybf.heights[0] = y_height_visible;
    ybf.strides[0] = y_stride;

    ybf.crop_widths[1] = (width + 1) >> 1; // uv_crop_width
    ybf.crop_heights[1] = (height + 1) >> 1; // uv_crop_height
    ybf.widths[1] = y_width >> 1;
    ybf.heights[1] = y_height_visible >> 1;
    ybf.strides[1] = uv_stride;

    ybf.border = h_border;
    ybf.frame_size = fb_size as usize;
    ybf.subsampling_x = 1;
    ybf.subsampling_y = 1;
    ybf.use_external_reference_buffers = 0;
    ybf.flags = if hbd != 0 { YV12_FLAG_HIGHBITDEPTH } else { 0 };

    ybf.buffers = [std::ptr::null_mut(); 3];
}

/// The layout of a frame of this size in the frame buffer `buf`, from where the buffer lies in the pool
/// (`base_offset`).
fn layout_frame_buffer(buf: &mut HwFrameBuffer, width: c_int, height: c_int, hbd: c_int) {
    let bpp = hbd + 1;
    let h_border = 16;
    let aligned_width = (width + 127) & !127;
    let aligned_height = (height + 127) & !127;
    let y_stride = aligned_width + 2 * h_border;
    let y_size = aligned_height * y_stride;
    let uv_stride = (aligned_width >> 1) + 2 * h_border;
    let uv_size = (aligned_height >> 1) * uv_stride;
    let y_width = (width + 7) & !7;
    let y_height_visible = (height + 7) & !7;
    buf.width = y_width;
    buf.height = y_height_visible;
    buf.hbd = hbd;
    buf.y_crop_width = width;
    buf.y_crop_height = height;
    buf.uv_crop_width = (width + 1) >> 1;
    buf.uv_crop_height = (height + 1) >> 1;
    buf.planes[0].stride = y_stride * bpp;
    buf.planes[1].stride = uv_stride * bpp;
    buf.planes[2].stride = uv_stride * bpp;
    buf.planes[0].offset = (buf.base_offset as c_int).wrapping_add(h_border * bpp);
    buf.planes[1].offset = (buf.base_offset as c_int).wrapping_add((y_size + h_border) * bpp);
    buf.planes[2].offset = buf.planes[1].offset + uv_size * bpp;
    for (plane, (stride, offset)) in buf.planes.iter_mut().zip(super::residual::plane_layout(width, height)) {
        plane.res_stride = stride;
        plane.res_offset = offset;
    }
}

/// `av1_reallocate_frame_buffer` (`dx/av1_core.h`): the frame buffer of the frame whose header is being
/// read: a buffer of the pool (the call waits for one when the pool is empty) with the layout of a frame
/// of this size. Returns -1 for a frame the decoder's buffers were not created for.
///
/// # Safety
/// `private` is null or the decoder; `ybf` is null or the buffer description of the frame.
pub(crate) unsafe extern "C" fn av1_reallocate_frame_buffer(
    private: *mut c_void,
    ybf: *mut YV12_BUFFER_CONFIG,
    width: c_int,
    height: c_int,
    upscaled_width: c_int,
    hbd: c_int,
) -> c_int {
    if ybf.is_null() || private.is_null() {
        return -1;
    }
    // SAFETY: the caller's contract.
    unsafe {
        let dec = private.cast::<Av1Core>();
        let ybf = &mut *ybf;
        ybf.hw_show_image = std::ptr::null_mut();
        if ybf.hw_buffer.is_null() {
            ybf.hw_buffer = (*dec).fb_pool.get();
        }
        if ybf.hw_buffer.is_null() {
            return -1;
        }
        if !frame_fits(&*dec, (*ybf.hw_buffer).size, width, height, upscaled_width, hbd) {
            (*dec).fb_pool.release(ybf.hw_buffer);
            ybf.hw_buffer = std::ptr::null_mut();
            return -1;
        }
        describe_frame(ybf, width, height, hbd);
        layout_frame_buffer(&mut *ybf.hw_buffer, width, height, hbd);
    }
    0
}

/// [`av1_reallocate_frame_buffer`] for a frame that takes its frame buffer when it is recorded
/// (`Av1Core::late_frame_buffer`, [`bind_frame_for_record`]): the description of the frame without a
/// buffer, and the same -1 for a frame the decoder's buffers were not created for. Nothing waits.
///
/// # Safety
/// `dec` is the decoder; `ybf` is the buffer description of the frame, which has no frame buffer.
pub(crate) unsafe fn describe_frame_without_buffer(
    dec: *mut Av1Core,
    ybf: *mut YV12_BUFFER_CONFIG,
    width: c_int,
    height: c_int,
    upscaled_width: c_int,
    hbd: c_int,
) -> c_int {
    // SAFETY: the caller's contract.
    unsafe {
        let ybf = &mut *ybf;
        ybf.hw_show_image = std::ptr::null_mut();
        // Every frame buffer of the pool has the size of the creation.
        if !frame_fits(&*dec, (*dec).fb_size as usize, width, height, upscaled_width, hbd) {
            return -1;
        }
        describe_frame(ybf, width, height, hbd);
    }
    0
}

/// `av1_release_fb_callback` (`dx/av1_core.h`): the entropy decoder lets go of a frame: its reference to
/// the frame buffer, and an output image the frame still had goes back to the application.
///
/// # Safety
/// `private` is the decoder; `ybf` is the buffer description of a frame of it.
pub(crate) unsafe extern "C" fn av1_release_fb_callback(private: *mut c_void, ybf: *mut YV12_BUFFER_CONFIG) {
    // SAFETY: the caller's contract.
    unsafe {
        let dec = private.cast::<Av1Core>();
        let ybf = &mut *ybf;
        if !ybf.hw_buffer.is_null() {
            (*dec).fb_pool.release(ybf.hw_buffer);
            ybf.hw_buffer = std::ptr::null_mut();
        }
        let img = ybf.hw_show_image;
        ybf.hw_show_image = std::ptr::null_mut();
        if let Some(img) = img.as_mut() {
            if let Some(release_image) = (*dec).cb_release_image {
                release_image((*dec).image_alloc_priv, img.alloc_priv);
            }
            img.fb_ptr = std::ptr::null_mut();
            img.size = 0;
            img.alloc_priv = std::ptr::null_mut();
            img.is_valid = 0;
            (*dec).image_pool.push(std::ptr::from_mut(img));
        }
    }
}

// ---------------------------------------------------------------------------------------------------------
// The setup of a frame.
// ---------------------------------------------------------------------------------------------------------

/// The slot's palette buffer: the pixels of the frame's palette blocks, written by the entropy decoder in
/// the layout of a frame buffer and copied into the frame by reconstruct_block.hlsl. A block can use a
/// palette only in a frame that allows screen content tools (`av1_allow_palette`), so the buffer is
/// created when the first such frame gets the slot and kept from then on. Returns false when the frame
/// does not have what it needs. Decode thread.
///
/// # Safety
/// `dec` is the decoder, `cm` the frame's header and `td` its slot.
unsafe fn palette_buffer_for_frame(dec: *mut Av1Core, cm: &AV1_COMMON, td: &mut av1_frame_thread_data) -> bool {
    if !td.palette_buffer.is_null() || cm.allow_screen_content_tools == 0 {
        return true;
    }
    // SAFETY: the caller's contract.
    unsafe {
        let size = (*dec).palette_size as u64;
        // The allocator after the creation: the thread that records the frames grows buffers with it.
        let _memory = (*dec).memory_lock.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(available_cb) = (*dec).memory_available_cb {
            let available = available_cb((*dec).memory_available_priv);
            if available >= 0 && size + MEMORY_GUARD_MARGIN_BUFFER > available as u64 {
                log(
                    true,
                    &format!(
                        "MEMORY REFUSED palette buffer of slot {} (frame {} allows screen content tools): {size} \
                         bytes ({:.1} MiB) and the margin of {:.1} MiB do not fit the {:.1} MiB the application has \
                         left for the decoder. The frame is a decode error; nothing was allocated and the D3D12 \
                         device is intact",
                        td.slot_index,
                        td.frame_number,
                        to_mib(size),
                        to_mib(MEMORY_GUARD_MARGIN_BUFFER),
                        to_mib(available as u64)
                    ),
                );
                return false;
            }
        }
        // A failure is logged by the allocator as "MEMORY FAILED buffer palette_buffer of slot n".
        let memory = &mut *(*dec).memory;
        let device = com_ref::<_, ID3D12Device>(&(*dec).compute.device);
        td.palette_buffer =
            memory.create_buffer(device, size as usize, DeviceUpload, c"palette_buffer", td.slot_index, false);
        if td.palette_buffer.is_null() {
            return false;
        }
        let total = memory.device_bytes();
        log(
            false,
            &format!(
                "MEMORY palette buffer of slot {} created for frame {}, the first on the slot that allows screen \
                 content tools: {:.1} MiB; device buffers now {:.1} MiB in {} buffers ({total} bytes)",
                td.slot_index,
                td.frame_number,
                to_mib(size),
                to_mib(total as u64),
                memory.device_count()
            ),
        );
    }
    true
}

/// The frame of `cm` on the slot `td` keeps its intra wavefront in the edges of its tiles
/// (`blocks::WaveEdges`): the slot gets the memory for them, if it has less than the frame needs, and the
/// frame its stamp. Nothing is cleared: what the frame reads of the edges it has written before
/// (`blocks::WavePlane`). Returns false when the process has no memory for them; the frame then keeps
/// the wavefront in the grid. Decode thread, before the tiles of the frame start.
fn wave_edges_for_frame(cm: &AV1_COMMON, td: &mut av1_frame_thread_data) -> bool {
    // The columns of luma a row of tiles keeps: those of the frame, to the end of a block that reaches
    // past its right edge. Chroma has half of them.
    let cols = (((cm.mi_cols + 31) & !31) + 32) as usize;
    let columns = cols * 3 / 2 * usize::try_from(cm.tile_rows).unwrap_or(0);
    let tiles = usize::try_from(td.tile_count).unwrap_or(0);
    let store = &mut td.wave;
    if store.above.len() < columns {
        let more = columns - store.above.len();
        if store.columns.try_reserve(more).is_err()
            || store.above.try_reserve(more).is_err()
            || store.above_end.try_reserve(more).is_err()
        {
            return false;
        }
        store.columns.resize(columns, WAVE_COLUMN_RESET);
        store.above.resize(columns, -1);
        store.above_end.resize(columns, 0);
    }
    if store.tiles.len() < tiles {
        if store.tiles.try_reserve(tiles - store.tiles.len()).is_err() {
            return false;
        }
        // SAFETY: pointers and numbers; setup_tile writes the pointers before a tile uses its edges.
        store.tiles.resize_with(tiles, || unsafe { std::mem::zeroed() });
    }
    store.frames += 1;
    td.wave_stamp = store.frames << 32;
    td.wave_cols = cols;
    td.wave_tiles = store.tiles.as_mut_ptr();
    td.wave_columns = store.columns.as_mut_ptr();
    td.wave_above = store.above.as_mut_ptr();
    td.wave_above_end = store.above_end.as_mut_ptr();
    // -poisonbuf: the edges hold what earlier frames left there, mostly plausible iterations. Filled
    // again for every frame, an entry that is read without having been written by this frame is "no
    // iteration" (the byte is negative) where the block there has one, and no decoded unit at all.
    // SAFETY: the four vectors, over the part this frame uses.
    unsafe {
        poison_constants(td.wave_tiles.cast(), size_of::<WaveEdges>() * tiles);
        poison_constants(td.wave_columns.cast(), size_of::<WaveColumn>() * columns);
        poison_constants(td.wave_above.cast(), size_of::<c_int>() * columns);
        poison_constants(td.wave_above_end.cast(), size_of::<i64>() * columns);
    }
    true
}

/// A frame that was set up and never submitted (a decode error between its setup and its submit) still
/// has its slot, or its output context, in `curr_frame_data`, with the frame buffers it counted. They are
/// taken back here, before the next frame or show_existing_frame takes a slot or a context of its own.
/// The thread that records the frames.
///
/// # Safety
/// `dec` is the decoder.
pub(crate) unsafe fn release_unsubmitted_frame(dec: *mut Av1Core) {
    // SAFETY: the caller's contract.
    unsafe {
        let td = std::mem::replace(&mut (*dec).curr_frame_data, std::ptr::null_mut());
        if !td.is_null() {
            release_frame(dec, td);
        }
    }
}

/// The slot (or output context) `td` of a frame that was set up and is not submitted goes back to its
/// pool, with the frame buffers the frame counted and its second slot.
///
/// # Safety
/// `dec` is the decoder and `td` a slot or output context of it that a frame took and that is in no
/// pool and in no work item; no thread parses into it any more.
pub(crate) unsafe fn release_frame(dec: *mut Av1Core, td: *mut av1_frame_thread_data) {
    // SAFETY: the caller's contract.
    unsafe {
        let td = &mut *td;
        {
            // With one frame in parse no thread waits for a buffer here: this is the thread that takes
            // them. With more, the thread that sets the next frame up may.
            let mut pool = (*dec).fb_pool.lock();
            let mut returned = false;
            if !td.back_buffer0.is_null() {
                returned |= pool.release(td.back_buffer0);
            }
            if !td.dst_frame_buffer.is_null() {
                returned |= pool.release(td.dst_frame_buffer);
            }
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
        }
        let td_ptr = std::ptr::from_mut(td);
        if td.output_only != 0 {
            // The output copy of a show_existing_frame: nothing but the reference to the frame it was to
            // copy.
            (*dec).output_data_pool.push(td_ptr);
            return;
        }
        super::pool_buffers::release(dec, td, false);
        super::mem_stats::frame_released(dec, td_ptr);
        // Its grids stay dirty and are not marked clean, so the next frame on this slot clears them itself.
        td.clear_pending = 0;
        (*dec).frame_data_pool.push(td_ptr);
    }
}

/// `get_ref_scale_factors` of `av1/common/onyxc_int.h`: the scale of a reference of the frame, or null.
fn ref_scale_factors(cm: &AV1_COMMON, ref_frame: usize) -> Option<&scale_factors> {
    let map_idx = cm.remapped_ref_idx[ref_frame - LAST_FRAME as usize];
    (map_idx != INVALID_IDX).then(|| &cm.ref_scale_factors[map_idx as usize])
}

/// `av1_is_scaled` of `av1/common/scale.h`.
fn is_scaled(sf: &scale_factors) -> bool {
    sf.x_scale_fp != REF_INVALID_SCALE
        && sf.y_scale_fp != REF_INVALID_SCALE
        && (sf.x_scale_fp != REF_NO_SCALE as c_int || sf.y_scale_fp != REF_NO_SCALE as c_int)
}

/// The description of a frame in another buffer: the layout of `fb` moved to where `buf` lies in the
/// pool.
fn describe_like(buf: &mut HwFrameBuffer, fb: &HwFrameBuffer, hbd: c_int) {
    buf.width = fb.width;
    buf.height = fb.height;
    buf.hbd = hbd;
    buf.y_crop_width = fb.y_crop_width;
    buf.uv_crop_width = fb.uv_crop_width;
    buf.y_crop_height = fb.y_crop_height;
    buf.uv_crop_height = fb.uv_crop_height;
    buf.planes = fb.planes;
    let moved = (buf.base_offset as c_int).wrapping_sub(fb.base_offset as c_int);
    for plane in &mut buf.planes {
        plane.offset += moved;
    }
}

/// `av1_setup_frame` (`dx/av1_core.h`), its first part: the frame takes a pipeline slot (the call waits
/// for one), the slot's grids are cleared, the frame's back buffer and its references are counted, and
/// the slot gets its palette buffer when the frame can have palette blocks. Returns 1 when that buffer
/// could not be had: the caller raises the decoder's error. `perf_start` gets the start of the setup's
/// timer for [`rav1g_setup_frame_end`].
///
/// # Safety
/// `pbi` is the entropy decoder's state with the header of the frame in `cm`; decode thread.
pub(crate) unsafe extern "C" fn rav1g_setup_frame_begin(
    pbi: *mut AV1Decoder,
    cm: *mut AV1_COMMON,
    perf_start: *mut i64,
) -> c_int {
    // SAFETY: the caller's contract.
    unsafe { setup_frame_begin(pbi, cm, perf_start, true).1 }
}

/// [`rav1g_setup_frame_begin`], which is this with `current`. Returns the slot of the frame and whether
/// the setup failed (1: no palette buffer).
///
/// `current`: the frame is the frame of the decoder, as it is with one frame in parse at a time: a
/// frame that was left in `curr_frame_data` is taken back first, the slot goes there, and the back
/// buffers the frames share are described for this frame here. Without it the frame is a value
/// (docs/phase2-rav1d.md, step 4): the caller keeps the slot, nothing of "the frame of the decoder" is
/// read or written, and the shared back buffers are described when the frame is recorded
/// ([`bind_frame_for_record`]), which is when its command list is the one that uses them.
///
/// # Safety
/// `pbi` is the entropy decoder's state with the header of the frame in `cm`; the thread that sets the
/// frames up (the decode thread).
pub(crate) unsafe fn setup_frame_begin(
    pbi: *mut AV1Decoder,
    cm: *mut AV1_COMMON,
    perf_start: *mut i64,
    current: bool,
) -> (*mut av1_frame_thread_data, c_int) {
    // SAFETY: the caller's contract, for all of this function.
    unsafe {
        let dec = (*pbi).gpu_decoder.cast::<Av1Core>();
        let perf_setup_start = super::perf::av1_perf_ticks();
        *perf_start = perf_setup_start;
        if perf_setup_start != 0 {
            (*super::perf::frame_acc(dec)).t_frame_start = perf_setup_start;
        }

        if current {
            release_unsubmitted_frame(dec);
        }

        let mut perf_t = super::perf::av1_perf_ticks();
        let td_ptr: *mut av1_frame_thread_data = (*dec).frame_data_pool.get();
        perf_add(dec, sys::AV1_PERF_CPU_WAIT_SLOT as usize, perf_t);
        let td = &mut *td_ptr;
        if current {
            (*dec).curr_frame_data = td_ptr;
        }
        td.frame_number = (*dec).frame_number;

        let cm = &mut *cm;
        let buf = &mut (*cm.cur_frame).buf;
        buf.hw_show_image = std::ptr::null_mut();

        cm.mi_grid_base = (*td.mode_info_grid).host_ptr.cast();
        cm.mi_grid_visible = cm.mi_grid_base;
        cm.mi_alloc_size = cm.mi_stride * ((cm.mi_rows + 31) & !31);
        // The two grids of the slot (this grid and the wavefront grid below; docs/phase2-rav1d.md, "No
        // clears by the CPU"). A frame keeps its intra wavefront in the edges of its tiles (below), so the
        // wavefront grid is not cleared by anybody. Only a frame that keeps it in the grid (no memory for
        // the edges) clears it, sized from the frame; behind such a frame the completion thread clears exactly those bytes again after the frame's fence and
        // records the size in the slot (`clean_iter_bytes`), and the next frame skips its clear when that
        // size is exactly the size it would clear itself. (Until interface version 3 switches moved these
        // clears to the decode thread, the completion thread for every frame, or the frame's command list:
        // `AV1_CLEARS_*`, retired.)
        //
        // The mode info grid is not cleared at all where a clear would change nothing a reader sees
        // (rav1d's parse): the parse of a frame writes the unit of
        // every 4x4 of the frame, so what the frames before left inside the frame is overwritten before
        // anything reads it (the sink reads a unit behind its block, the block lists read units of blocks
        // that are parsed, the stages run when the parse is through; a frame whose parse fails is not
        // submitted). What a clear is still needed for is the part of the grid outside the frame, which
        // the stages may read and which has to be null: it is null when the grid was cleared once for
        // these bytes and only frames of this width, height and row length were parsed into it since. So
        // the slot remembers those (`kept_grid`), and a frame of another size clears as before. That is
        // 4.2 MB a 4K frame the CPU does not write (docs/phase2-rav1d.md).
        let mi_grid_bytes = size_of::<sys::MiGridEntry>() * cm.mi_alloc_size as usize;
        let grid_frame = [cm.mi_cols, cm.mi_rows, cm.mi_stride];
        let kept = td.kept_grid == grid_frame && td.kept_grid_bytes == mi_grid_bytes;
        if !kept {
            perf_t = super::perf::av1_perf_ticks();
            cm.mi_grid_base.cast::<u8>().write_bytes(0, mi_grid_bytes);
            perf_add_memset(dec, sys::AV1_PERF_CPU_MEMSET as usize, perf_t, mi_grid_bytes);
        }
        (td.kept_grid, td.kept_grid_bytes) = (grid_frame, mi_grid_bytes);
        let clean_iter_bytes = td.clean_iter_bytes;
        td.clean_iter_bytes = 0;
        td.clear_iter_bytes = 0;
        td.clear_pending = 1;

        let (y_width, y_height) = (buf.widths[0], buf.heights[0]);
        let grid_w = (((y_width + 63) & !63) >> 2) + 2 + 128;
        let grid_h = (((y_height + 63) & !63) >> 2) + 2 + 128;
        td.iter_grid_stride = grid_w;
        td.iter_grid_stride_uv = grid_w >> 1;
        td.bitdepth = buf.bit_depth as c_int;

        td.tile_count = cm.tile_cols * cm.tile_rows;
        td.thread_count = (*pbi).max_threads.min(td.tile_count);
        if let Some(pools) = (*dec).pools.as_deref() {
            // The frame is the youngest of those that hold pieces of the pools; its tiles take theirs as
            // they are parsed.
            let tiles = usize::try_from(td.tile_count).unwrap_or(0);
            td.pieces.resize_with(tiles.max(td.pieces.len()), TilePieces::default);
            for pieces in &mut td.pieces {
                pieces.mode_info.clear();
                pieces.txb.clear();
                pieces.piece = [(0, 0); 3];
                pieces.chunks_taken = [0; 3];
                pieces.cells_left = 0;
                pieces.coef_values = 0;
                pieces.coef_run = 0;
            }
            td.tile_pieces = td.pieces.as_mut_ptr();
            td.index_lists.resize_with(tiles.max(td.index_lists.len()), Vec::new);
            td.tile_index_lists = td.index_lists.as_mut_ptr();
            pools.begin(td.slot_index as usize, frame_cells(cm));
        }
        // Inter breakdown (-gputimedetail): the tile threads count the blocks they push, each tile into its
        // own counters, which the prediction stage adds up when the frame is recorded.
        td.perf_inter_mode = perf_detail_mode();
        if td.perf_inter_mode != 0 {
            // Zeroed counters for the tiles of this frame; the vector keeps its memory from frame to
            // frame of the slot. Without memory for them the frame has no breakdown.
            let tiles = usize::try_from(td.tile_count).unwrap_or(0);
            let counters = &mut td.perf_inter_tiles;
            counters.clear();
            if counters.try_reserve(tiles).is_ok() {
                // SAFETY: the counters are 64-bit numbers.
                counters.resize(tiles, std::mem::zeroed());
            } else {
                td.perf_inter_mode = 0;
            }
        }

        // Intra step timing: the tiles count their steps while the switch is on.
        super::step_time::setup_frame(td);

        td.gen_intra_iter_y = (*td.gen_intra_inter_grid).host_ptr.cast();
        td.iter_grid_offset_uv = grid_w * grid_h;
        td.gen_intra_iter_uv = td.gen_intra_iter_y.add(td.iter_grid_offset_uv as usize);
        td.intra_stats = [0; 4];
        td.intra_push_loops = 0;
        // The tiles keep the wavefront in their edges; av1_mi_push_block() writes the one entry per block
        // of the grid that gen_pred_blocks.hlsl reads, and nothing of it is cleared, here or behind the
        // frame.
        td.wave_edges = c_int::from(wave_edges_for_frame(cm, td));
        if td.wave_edges == 0 {
            // The wavefront in the grid, which av1_mi_push_block() reads back: every entry no block of
            // the frame writes has to be "no iteration".
            // The luma grid (4 bytes per entry) and the chroma grid right behind it: one range from the
            // start of the buffer. Skipped when exactly this range was cleared behind the frame before.
            let iter_grid_bytes = td.iter_grid_offset_uv as usize * 5;
            if clean_iter_bytes != iter_grid_bytes {
                perf_t = super::perf::av1_perf_ticks();
                td.gen_intra_iter_y.cast::<u8>().write_bytes(0xFF, td.iter_grid_offset_uv as usize * 4);
                td.gen_intra_iter_uv.cast::<u8>().write_bytes(0xFF, td.iter_grid_offset_uv as usize);
                perf_add_memset(dec, sys::AV1_PERF_CPU_MEMSET as usize, perf_t, iter_grid_bytes);
            }
            td.clear_iter_bytes = iter_grid_bytes;
        }

        td.is_hbd = c_int::from(buf.bit_depth > 8);
        let lib = (*dec).shader_lib;
        td.shaders = if td.is_hbd != 0 { &raw mut (*lib).shaders_hbd } else { &raw mut (*lib).shaders_8bit };

        // The layout of the frame, for its palette blocks: the parse writes them, and a frame in parse
        // may not have its frame buffer yet, or share its back buffer with the frame that is recorded.
        td.layout = std::mem::zeroed();
        td.layout.size = (*dec).fb_size as usize;
        layout_frame_buffer(&mut td.layout, buf.crop_widths[0], buf.crop_heights[0], td.is_hbd);

        // A frame that is a value takes its frame buffer when it is recorded, in a decoder that was
        // created for that (`bind_frame_for_record`).
        let late = !current && (*dec).late_frame_buffer != 0;
        td.late_buffers = c_int::from(late);
        let fb_ptr = buf.hw_buffer;
        assert!(late == fb_ptr.is_null(), "the frame has its frame buffer, or takes it when it is recorded");
        if let Some(fb) = fb_ptr.as_mut() {
            fb.frame_number = (*dec).frame_number;
            fb.hbd = td.is_hbd;
        }
        (*dec).frame_number += 1;
        td.do_superres = c_int::from((*dec).enable_superres != 0 && cm.width != cm.superres_upscaled_width);
        td.do_loop_rest =
            c_int::from(cm.rst_info.iter().any(|plane| plane.frame_restoration_type != RESTORE_NONE as _));
        td.do_cdef = c_int::from(
            cm.skip_loop_filter == 0
                && cm.coded_lossless == 0
                && (cm.cdef_info.cdef_bits != 0
                    || cm.cdef_info.cdef_strengths[0] != 0
                    || cm.cdef_info.cdef_uv_strengths[0] != 0),
        );
        td.do_filmgrain = c_int::from(cm.film_grain_params.apply_grain != 0);
        td.dst_frame_buffer = fb_ptr;
        td.back_buffer0 = std::ptr::null_mut();
        if late {
            // No buffer and no reference is counted for the frame until it is recorded: its slot has
            // none, which is also what a frame that fails before that gives back.
            td.frame_buffer = std::ptr::null_mut();
            td.refs = [std::ptr::null_mut(); 8];
        } else if td.do_loop_rest != 0 || td.do_cdef != 0 || td.do_superres != 0 {
            // The frame is reconstructed into a back buffer and filtered from there into its own buffer.
            // Only its own command list reads or writes the back buffer, and the command lists of the frames
            // run on the queue one after the other, so one buffer can serve all frames (shared_back_buffer),
            // as back_buffer1 does for the filters: its description is rewritten here for every frame, after
            // the frame before was recorded. Otherwise the frame takes a buffer from the pool and returns it
            // when its GPU work is done.
            let mut new_buf = &raw mut (*dec).back_buffer0;
            if (*dec).shared_back_buffer == 0 {
                perf_t = super::perf::av1_perf_ticks();
                new_buf = (*dec).fb_pool.get();
                perf_add(dec, sys::AV1_PERF_CPU_WAIT_FB as usize, perf_t);
                td.back_buffer0 = new_buf;
            }
            td.frame_buffer = new_buf;
            // The two buffers are described for this frame: the back buffer, and back_buffer1, which
            // gets the whole description of the frame like the back buffer: CDEF writes into it when the
            // frame has superres and no loop restoration, and the upscale then reads its visible width
            // as the width it scales from. (It used to get the layout only; the visible size was
            // whatever an earlier frame had left there, zero at first, and the upscale of such a frame
            // scaled from that width.) A frame that is a value is set up while an earlier frame may be
            // recorded with these buffers: for it they are described when it is recorded itself.
            if current {
                describe_back_buffers(dec, td);
            }
        } else {
            td.frame_buffer = td.dst_frame_buffer;
        }

        if let Some(fb) = fb_ptr.as_mut() {
            // The reference counts are the pool's: counted under its lock.
            let counting = (*dec).fb_pool.lock();
            fb.ref_cnt += 1;
            for i in 0..7 {
                td.refs[i] = std::ptr::null_mut();
                if cm.remapped_ref_idx[i] == -1 {
                    continue;
                }
                let reference = cm.ref_frame_map[cm.remapped_ref_idx[i] as usize];
                if reference.is_null() {
                    continue;
                }
                let hw_buffer = (*reference).buf.hw_buffer;
                assert!((*hw_buffer).size == (*dec).fb_size as usize, "a reference is a frame buffer of the pool");
                td.refs[i] = hw_buffer;
                (*hw_buffer).ref_cnt += 1;
            }
            drop(counting);
        }
        super::mem_stats::frame_setup(dec, cm, td);

        // From here on the slot is in a state the next setup can take back (the frame's buffers and
        // references are counted), so a failure below is reported as a decode error of this frame.
        (td_ptr, c_int::from(!palette_buffer_for_frame(dec, cm, td)))
    }
}

/// The back buffers the frames share (`back_buffer0` with the shared back buffer, `back_buffer1`) get
/// the description of the frame on `td`, which is reconstructed into a back buffer.
///
/// # Safety
/// `dec` is the decoder and `td` the slot of a frame that was set up with a back buffer; no other
/// frame's command list is being recorded with these buffers.
unsafe fn describe_back_buffers(dec: *mut Av1Core, td: &av1_frame_thread_data) {
    // SAFETY: the caller's contract; the frame buffer of the frame is its own until its GPU work is done.
    unsafe {
        let fb = &*td.dst_frame_buffer;
        describe_like(&mut *td.frame_buffer, fb, td.is_hbd);
        describe_like(&mut (*dec).back_buffer1, fb, td.is_hbd);
    }
}

/// A frame that is a value (`setup_frame_begin` without `current`) is about to be recorded: its slot is
/// the one the stages record from, and the shared back buffers are described for it. A frame that has
/// no frame buffer yet (`Av1Core::late_frame_buffer`) takes it here, with `cur`, its description, and
/// `refs`, those of its references.
///
/// # Safety
/// `dec` is the decoder and `td` the slot of a frame that was set up and whose tiles are parsed; `cur`
/// is the description of the frame and `refs` are null or the descriptions of its references, each
/// with a reference of the caller's, and the frames of `refs` are recorded; the thread that records
/// the frames, which has no other frame in `curr_frame_data`.
pub(crate) unsafe fn bind_frame_for_record(
    dec: *mut Av1Core,
    td: *mut av1_frame_thread_data,
    cur: *mut RefCntBuffer,
    refs: &[*mut RefCntBuffer; 7],
) {
    // SAFETY: the caller's contract.
    unsafe {
        if (*td).late_buffers != 0 {
            take_frame_buffers(dec, &mut *td, cur, refs);
        }
        (*dec).curr_frame_data = td;
        if (*td).frame_buffer != (*td).dst_frame_buffer {
            describe_back_buffers(dec, &*td);
        }
    }
}

/// The frame on the slot `td`, which was set up without a frame buffer, takes its buffers of the pool
/// and counts its references, as [`setup_frame_begin`] does for a frame that has its buffer by then.
///
/// The take waits while the pool is empty. What is out of the pool then is held by the reference
/// slots and the frames of the queue, or only by frames on the GPU. The first are at most 8 buffers:
/// each was in a reference slot when this frame, the oldest of the queue, began, and no frame that
/// began since has a buffer. With this frame's own that is within the pool, so a buffer the take
/// waits for is one a frame on the GPU returns, which the completion thread does without this thread
/// (`ResourceConfig::with_slots`). The frame is reconstructed into its own buffer or into the back
/// buffer the frames share: a decoder whose frames take a back buffer from the pool does not have
/// frames that come here.
///
/// # Safety
/// As [`bind_frame_for_record`], for a frame with `late_buffers`.
unsafe fn take_frame_buffers(
    dec: *mut Av1Core,
    td: &mut av1_frame_thread_data,
    cur: *mut RefCntBuffer,
    refs: &[*mut RefCntBuffer; 7],
) {
    // SAFETY: the caller's contract. The frame buffer of the description is written here while other
    // threads may read the description: none of them reads its frame buffer before this frame is
    // recorded (a frame that refers to it counts that reference here as well, when its own turn comes;
    // the thread of the decode calls asks the description's mark, not its buffer).
    unsafe {
        let (width, height) = ((*cur).buf.crop_widths[0], (*cur).buf.crop_heights[0]);
        let filtered = td.do_loop_rest != 0 || td.do_cdef != 0 || td.do_superres != 0;
        let perf_t = super::perf::av1_perf_ticks();
        assert!((*dec).shared_back_buffer != 0, "frames take their buffer here only with the shared back buffer");
        let fb_ptr = (*dec).fb_pool.get();
        perf_add(dec, sys::AV1_PERF_CPU_WAIT_FB as usize, perf_t);
        let fb = &mut *fb_ptr;
        layout_frame_buffer(fb, width, height, td.is_hbd);
        fb.frame_number = td.frame_number;
        (*cur).buf.hw_buffer = fb_ptr;
        td.dst_frame_buffer = fb_ptr;
        td.frame_buffer = if filtered { &raw mut (*dec).back_buffer0 } else { fb_ptr };
        {
            // The reference counts are the pool's: counted under its lock.
            let _counting = (*dec).fb_pool.lock();
            fb.ref_cnt += 1;
            for (held, &reference) in td.refs.iter_mut().zip(refs) {
                *held = std::ptr::null_mut();
                if reference.is_null() {
                    continue;
                }
                let hw_buffer = (*reference).buf.hw_buffer;
                assert!(
                    !hw_buffer.is_null() && (*hw_buffer).size == (*dec).fb_size as usize,
                    "a reference is a frame buffer of the pool"
                );
                *held = hw_buffer;
                (*hw_buffer).ref_cnt += 1;
            }
        }
        td.late_buffers = 0;
        super::mem_stats::frame_buffers_taken(dec);
    }
}

/// `av1_setup_frame`, its last part: the completion thread runs, and the slot knows the scale of the
/// frame's references and which of them a global motion block is warped from.
///
/// # Safety
/// As for [`rav1g_setup_frame_begin`], after it; `perf_start` is what that part returned.
pub(crate) unsafe extern "C" fn rav1g_setup_frame_end(
    pbi: *mut AV1Decoder,
    cm: *mut AV1_COMMON,
    perf_start: i64,
) -> c_int {
    // SAFETY: the caller's contract.
    unsafe {
        let dec = (*pbi).gpu_decoder.cast::<Av1Core>();
        setup_frame_end(dec, (*dec).curr_frame_data, cm, perf_start)
    }
}

/// [`rav1g_setup_frame_end`] for the frame on the slot `td`.
///
/// # Safety
/// `dec` is the decoder, `td` the slot [`setup_frame_begin`] returned for the frame of `cm`, and
/// `perf_start` the start it returned.
pub(crate) unsafe fn setup_frame_end(
    dec: *mut Av1Core,
    td: *mut av1_frame_thread_data,
    cm: *const AV1_COMMON,
    perf_start: i64,
) -> c_int {
    // SAFETY: the caller's contract.
    unsafe {
        let td = &mut *td;
        let cm = &*cm;
        assert!(td.thread_count <= EntropyThreadCount as c_int, "more tile threads than the decoder has room for");
        if !start_completion_thread(dec) {
            return -1;
        }

        td.scale_enable = 0;
        for i in LAST_FRAME as usize..=ALTREF_FRAME as usize {
            let sf = ref_scale_factors(cm, i);
            match sf {
                None => {
                    td.scale_factors[i].x_scale = REF_NO_SCALE as c_int;
                    td.scale_factors[i].y_scale = REF_NO_SCALE as c_int;
                    td.scale_factors[i].x_step = 0;
                    td.scale_factors[i].y_step = 0;
                }
                Some(sf) => {
                    td.scale_factors[i].x_step = sf.x_step_q4;
                    td.scale_factors[i].y_step = sf.y_step_q4;
                    td.scale_factors[i].x_scale = sf.x_scale_fp;
                    td.scale_factors[i].y_scale = sf.y_scale_fp;
                    td.scale_enable |= c_int::from(is_scaled(sf));
                }
            }
            // Global motion warp from this reference: the model, and a reference that is not scaled.
            // libaom's av1_allow_warp() refuses the warp of a scaled reference before it looks at the model,
            // and so does the specification (useWarp, 7.11.3.1: is_scaled(refFrame) is 0); the block is then
            // predicted from its motion vector, like a block of a frame without the model. A local warp
            // needs no such test here: a block with a scaled reference cannot be coded as one
            // (motion_mode_allowed of blockd.h).
            let gm = &cm.global_motion[i];
            td.gm_warp[i] =
                c_int::from(gm.wmtype as c_int > TRANSLATION && gm.invalid == 0 && !sf.is_some_and(is_scaled));
        }
        // -interfast: not for a frame with a scaled reference, whose blocks all go through the inter_scale
        // shaders.
        td.inter_fast = if (*dec).inter_fast_tiles > 0 && td.scale_enable == 0 { inter_fast_mode() } else { 0 };
        perf_add(dec, sys::AV1_PERF_CPU_SETUP as usize, perf_start);
    }
    0
}

/// What a tile thread starts a tile with ([`setup_tile`]).
pub(crate) struct TileSetup {
    /// The tile's cursors into the buffers of the frame's slot.
    pub tile_data: *mut av1_tile_data,
}

/// A tile of the frame on the slot `td` starts: its cursors into the buffers of the frame's slot (block
/// index list, block maps) and into the pools, of which it has nothing yet: it takes its first piece of
/// each with its first block and its first transform block ([`more_mode_info`], [`more_coefs`],
/// [`more_transform_blocks`]).
///
/// # Safety
/// `dec` is the decoder while the tiles of the frame of `cm`, which has the slot `td`, are decoded; the
/// calling thread is the one that decodes `tile`.
pub(crate) unsafe fn setup_tile(
    dec: *mut Av1Core,
    td: *mut av1_frame_thread_data,
    cm: &AV1_COMMON,
    tile: &TileInfo,
) -> TileSetup {
    // SAFETY: the caller's contract, for all of this function.
    unsafe {
        let tile_id = tile.tile_col + tile.tile_row * cm.tile_cols;
        // The tile's counts of the intra step timing; the vector of vectors is not touched while the tiles
        // of the frame run.
        let perf_steps = super::step_time::tile_counts(td, tile_id as usize);
        let td = &*td;
        let t_ptr = td.tile_data.add(tile_id as usize);
        let mut perf_t = super::perf::av1_perf_ticks();
        t_ptr.write_bytes(0, 1);
        perf_add_memset(dec, sys::AV1_PERF_CPU_MEMSET_TILE as usize, perf_t, size_of::<av1_tile_data>());
        let t = &mut *t_ptr;
        if td.perf_inter_mode != 0 && (tile_id as usize) < td.perf_inter_tiles.len() {
            // The tile's own counters; the vector is not touched while the tiles of the frame run.
            t.perf_inter = td.perf_inter_tiles.as_ptr().add(tile_id as usize).cast_mut().cast();
        }
        t.perf_steps = perf_steps;
        let tile_rows = (tile.mi_row_end - tile.mi_row_start + 15) & !15;
        let tile_cols = (tile.mi_col_end - tile.mi_col_start + 15) & !15;
        let mi_offset = tile.mi_row_start * ((cm.mi_cols + 15) & !15) + tile.mi_col_start * tile_rows;

        t.intra_iter_max = -1;
        t.intra_iter_max_uv = -1;
        t.intrabc_sb = -1;
        t.mi_count = 0;
        t.dq_buffer_offset = 0;
        t.dq_buffer_ptr = 0;
        t.dq_buffer_max = 0;
        t.dq_buffer_size = 0;
        t.dq_buffer_end = 0;
        t.blocks_offset = 0;
        t.mi_offset = 0;
        let pieces = &mut *td.tile_pieces.add(tile_id as usize);
        pieces.col_end = tile.mi_col_end.min(cm.mi_cols);
        pieces.row_end = tile.mi_row_end.min(cm.mi_rows);
        pieces.cells_left = cells(pieces.col_end - tile.mi_col_start) * cells(pieces.row_end - tile.mi_row_start);
        pieces.cells = pieces.cells_left as usize;
        let setup = TileSetup { tile_data: t_ptr };

        t.mi_col_start = tile.mi_col_start;
        t.mi_row_start = tile.mi_row_start;
        // The tile's block index list is its own memory (`av1_frame_thread_data::index_lists`) and grows
        // with its blocks (`av1_mi_push_block`); where it starts in the slot's buffer is known when the
        // frame is recorded.
        t.gen_index_ptr = 0;
        t.gen_index_base = 0;
        let indexes = &mut *td.tile_index_lists.add(tile_id as usize);
        indexes.clear();
        t.gen_indexes = indexes.as_mut_ptr();
        t.gen_block_warp_offset = tile_id * (256 + 32) + (mi_offset >> 2) * 10;
        t.gen_block_map_offset = t.gen_block_warp_offset + 32;
        let block_map = (*td.gen_block_map).host_ptr.cast::<c_int>();
        t.gen_block_map = block_map.offset(t.gen_block_map_offset as isize);
        t.gen_block_map_wrp = block_map.offset(t.gen_block_warp_offset as isize);
        // -interfast: the tile's lists by motion vector class, behind the block maps of all tiles.
        if td.inter_fast != 0 && tile_id < (*dec).inter_fast_tiles {
            t.gen_block_fast_offset = (*dec).pred_map_size + tile_id * InterFastMapSize;
            t.gen_block_map_fast = block_map.offset(t.gen_block_fast_offset as isize);
            t.gen_block_map_fast.write_bytes(0, InterFastMapSize as usize);
        }
        t.intra_single_edge = 0;
        t.gen_intra_iter_y = -1;
        t.gen_intra_iter_uv = -1;
        t.have_inter = 0;
        // The slot of the tile's frame, for av1_mi_push_block.
        t.frame_data = std::ptr::from_ref(td).cast_mut().cast();
        t.gen_intra_max_iter =
            if td.tile_count == 1 { ((*dec).pred_map_size - 512) / 10 } else { ((tile_cols * tile_rows) >> 2) - 2 };
        t.gen_intra_iter_set = (t.gen_intra_max_iter - 16).min(256);
        t.gen_iter_clear_offset = (t.gen_intra_iter_set + 16) * 10 + 256;
        t.gen_iter_clear_size =
            (size_of::<c_int>() as c_int).wrapping_mul(256 + t.gen_intra_max_iter * 10 - t.gen_iter_clear_offset);
        perf_t = super::perf::av1_perf_ticks();
        let clear_ints = (32 + 16 + t.gen_iter_clear_offset) as usize;
        t.gen_block_map_wrp.write_bytes(0, clear_ints);
        perf_add_memset(dec, sys::AV1_PERF_CPU_MEMSET_TILE as usize, perf_t, size_of::<c_int>() * clear_ints);
        if td.wave_edges != 0 {
            // The edges of the tile's wavefront, with the rows above it in the part of its row of tiles:
            // tiles side by side have columns of their own there. Nothing of them is cleared.
            let edges = &mut *td.wave_tiles.add(tile_id as usize);
            edges.start(tile.mi_col_start, tile.mi_col_end - tile.mi_col_start);
            let first = td.wave_cols * 3 / 2 * tile.tile_row as usize;
            for (plane, at) in edges.planes.iter_mut().zip([0, td.wave_cols]) {
                plane.above = td.wave_columns.add(first + at);
            }
            for (plane, at) in edges.legacy.iter_mut().zip([0, td.wave_cols]) {
                plane.above = td.wave_above.add(first + at);
                plane.above_end = td.wave_above_end.add(first + at);
                plane.stamp = td.wave_stamp;
            }
            t.wave_edges = std::ptr::from_mut(edges).cast();
        }
        setup
    }
}

// ---------------------------------------------------------------------------------------------------------
// The pools (pool.rs): what a tile takes as it is parsed.
// ---------------------------------------------------------------------------------------------------------

/// The cells (64x64 areas) along `units` 4x4 units.
fn cells(units: c_int) -> c_int {
    (units.max(0) + 15) >> 4
}

/// The cells of the frame of `cm`: those of its tiles, each from its own corner. A tile begins at a
/// superblock, which is a cell or four, so that is the cells of the frame but for a tile that ends at
/// the frame's edge.
fn frame_cells(cm: &AV1_COMMON) -> usize {
    let log2 = cm.seq_params.mib_size_log2;
    let along = |starts: &[c_int], tiles: c_int, units: c_int| -> c_int {
        (0..usize::try_from(tiles).unwrap_or(0))
            .map(|tile| cells((starts[tile + 1] << log2).min(units) - (starts[tile] << log2)))
            .sum()
    };
    let across = along(&cm.tile_col_start_sb, cm.tile_cols, cm.mi_cols);
    let down = along(&cm.tile_row_start_sb, cm.tile_rows, cm.mi_rows);
    usize::try_from(across * down).unwrap_or(0)
}

/// A piece of a pool for a tile of the frame on the slot `td`: its first chunk and its chunks. Waits
/// until the frame may have one and one is free (`Pools::take_growing`). `None`: the frame does not fit the
/// pool, or the decoder is being released; the frame fails.
///
/// # Safety
/// `dec` is a decoder with pools while the tiles of the frame on the slot `td` are parsed; `pieces` is
/// the calling thread's tile.
unsafe fn take_piece(
    dec: *mut Av1Core,
    td: *const av1_frame_thread_data,
    pieces: &mut TilePieces,
    kind: Kind,
) -> Option<(usize, usize, *mut GpuBufferObject)> {
    // SAFETY: the caller's contract.
    unsafe {
        let pools = (*dec).pools.as_deref()?;
        let k = kind as usize;
        // A piece, and never more than the tile can still need: a chunk is the worst case of a cell.
        let limit = pieces.cells - pieces.chunks_taken[k];
        let want = pools.piece(kind, usize::try_from(pieces.cells_left).unwrap_or(0)).min(limit);
        if want == 0 {
            return None;
        }
        let perf_t = super::perf::av1_perf_ticks();
        let worst = super::pool_buffers::worst(&*dec);
        let taken = pools.take_growing((*td).slot_index as usize, kind, want, worst, |chunks| {
            super::pool_buffers::grow(dec, td, kind, chunks)
        });
        perf_add(dec, sys::AV1_PERF_CPU_WAIT_SEC_SLOT as usize, perf_t);
        match taken {
            Take::Piece { first, chunks, grown } => {
                pieces.piece[k] = (first, chunks);
                pieces.chunks_taken[k] += chunks;
                Some((first, chunks, super::pool_buffers::backing(dec, kind, grown)))
            }
            Take::Full => {
                let what = match kind {
                    Kind::ModeInfo => "blocks do not fit the mode info pool",
                    Kind::Coefs => "coefficients do not fit the coefficient pool",
                    Kind::Txb => "transform blocks do not fit the pool of their records",
                };
                // In a pool below the worst case the frames in parse behind this one may hold a part.
                let behind = pools.held_behind((*td).slot_index as usize, kind);
                log(
                    true,
                    &format!(
                        "MEMORY FAILED frame {}: its {what} ({} chunks of {} items, of which the frames in parse \
                         behind it hold {behind}); the frame is a decode error",
                        (*td).frame_number,
                        pools.stats().total[k],
                        CHUNK_ITEMS[k]
                    ),
                );
                None
            }
            Take::Refused | Take::Stopped => None,
        }
    }
}

/// The tile `tile_id` of the frame on the slot `td` has filled the mode info records it had: its next
/// piece of the pool, as the records and how many there is room for.
///
/// # Safety
/// As [`take_piece`]; `t` is the tile's data.
pub(crate) unsafe fn more_mode_info(
    dec: *mut Av1Core,
    td: *const av1_frame_thread_data,
    tile_id: usize,
    t: &mut av1_tile_data,
) -> Option<(*mut MB_MODE_INFO, c_int)> {
    // SAFETY: the caller's contract; the piece is inside the pool's buffer.
    unsafe {
        let pieces = &mut *(*td).tile_pieces.add(tile_id);
        let (first, chunks, backing) = take_piece(dec, td, pieces, Kind::ModeInfo)?;
        let run = Run {
            first: (first * CHUNK_ITEMS[Kind::ModeInfo as usize]) as c_int,
            count: (chunks * CHUNK_ITEMS[Kind::ModeInfo as usize]) as c_int,
        };
        // The pieces before this one are full; the count of this one is set at the tile's end.
        pieces.mode_info.push(run);
        t.mi_offset = run.first;
        t.mi_count = 0;
        let records = (*backing).host_ptr.cast::<MB_MODE_INFO>().add(run.first as usize);
        if (*dec).gpu_grid == 0 {
            // With a CPU grid, neighbouring blocks may live in different mappings. The pool's
            // allocation gives this tile exclusive access to these chunk entries until its fence.
            let map = (*dec).mi_chunks.as_ptr().add(first);
            for chunk in 0..chunks {
                (*map.add(chunk)).set(records.add(chunk * CHUNK_ITEMS[Kind::ModeInfo as usize]));
            }
        }
        Some((records, run.count))
    }
}

/// The same for the tile's transform block records: the tile's cursor moves to the next piece. Returns
/// the records there is room for; 0: none, the frame fails.
///
/// # Safety
/// As [`more_mode_info`].
pub(crate) unsafe fn more_transform_blocks(
    dec: *mut Av1Core,
    td: *const av1_frame_thread_data,
    tile_id: usize,
    t: &mut av1_tile_data,
) -> c_int {
    // SAFETY: the caller's contract; the piece is inside the pool's buffer.
    unsafe {
        let pieces = &mut *(*td).tile_pieces.add(tile_id);
        // The piece the tile has written, which is full.
        if let Some(last) = pieces.txb.last_mut() {
            last.count = t.idct_blocks_ptr;
        }
        let Some((first, chunks, backing)) = take_piece(dec, td, pieces, Kind::Txb) else {
            return 0;
        };
        let run = Run {
            first: (first * CHUNK_ITEMS[Kind::Txb as usize]) as c_int,
            count: (chunks * CHUNK_ITEMS[Kind::Txb as usize]) as c_int,
        };
        pieces.txb.push(Run { first: run.first, count: 0 });
        t.blocks_offset = run.first;
        t.idct_blocks_host = (*backing).host_ptr.cast::<crate::sys::tx_block_info_gpu>().add(run.first as usize);
        t.idct_blocks_ptr = 0;
        run.count
    }
}

/// The coefficients of the tile's next transform block do not fit what is left of its piece of the
/// coefficient pool: the tile's cursor moves to the next piece, or the piece grows by the next one
/// when that follows it. Returns false when there is none: the frame fails.
///
/// # Safety
/// As [`more_mode_info`].
pub(crate) unsafe fn more_coefs(
    dec: *mut Av1Core,
    td: *const av1_frame_thread_data,
    tile_id: usize,
    t: &mut av1_tile_data,
) -> bool {
    // SAFETY: the caller's contract.
    unsafe {
        let pieces = &mut *(*td).tile_pieces.add(tile_id);
        let Some((first, chunks, backing)) = take_piece(dec, td, pieces, Kind::Coefs) else {
            return false;
        };
        let start = (first * COEF_CHUNK) as c_int;
        let base = (*backing).host_ptr.cast::<i32>();
        if t.dq_buffer_end != start || t.dq_buffer_base != base {
            // What is left of the piece before stays empty, and zero.
            pieces.coef_values += i64::from(t.dq_buffer_ptr - pieces.coef_run);
            pieces.coef_run = start;
            t.dq_buffer_ptr = start;
        }
        t.dq_buffer_base = base;
        t.dq_buffer_end = start + (chunks * COEF_CHUNK) as c_int;
        true
    }
}

/// A superblock of the tile at (`x4`, `y4`) is parsed: its cells are no longer what the tile, and the
/// frame, can still need of the pools.
///
/// # Safety
/// As [`more_mode_info`].
pub(crate) unsafe fn superblock_parsed(
    dec: *mut Av1Core,
    td: *const av1_frame_thread_data,
    tile_id: usize,
    x4: c_int,
    y4: c_int,
    superblock: c_int,
) {
    // SAFETY: the caller's contract.
    unsafe {
        let Some(pools) = (*dec).pools.as_deref() else {
            return;
        };
        let pieces = &mut *(*td).tile_pieces.add(tile_id);
        let parsed = cells(superblock.min(pieces.col_end - x4)) * cells(superblock.min(pieces.row_end - y4));
        let parsed = parsed.min(pieces.cells_left);
        if parsed > 0 {
            pieces.cells_left -= parsed;
            pools.progress((*td).slot_index as usize, parsed as usize);
        }
    }
}

/// The tile is parsed: its last pieces have their counts, and the chunks of them that it did not write
/// go back to the pools.
///
/// # Safety
/// As [`more_mode_info`], behind the tile's last block.
pub(crate) unsafe fn tile_parsed(
    dec: *mut Av1Core,
    td: *const av1_frame_thread_data,
    tile_id: usize,
    t: &av1_tile_data,
) {
    // SAFETY: the caller's contract.
    unsafe {
        let Some(pools) = (*dec).pools.as_deref() else {
            return;
        };
        let slot = (*td).slot_index as usize;
        let pieces = &mut *(*td).tile_pieces.add(tile_id);
        let shrink = |kind: Kind, piece: &mut (usize, usize), used: c_int| {
            let keep = (usize::try_from(used).unwrap_or(0)).div_ceil(CHUNK_ITEMS[kind as usize]);
            if piece.1 != 0 && keep < piece.1 {
                pools.shrink(slot, kind, piece.0, keep);
                piece.1 = keep;
            }
        };
        if let Some(last) = pieces.mode_info.last_mut() {
            last.count = t.mi_count;
            shrink(Kind::ModeInfo, &mut pieces.piece[Kind::ModeInfo as usize], t.mi_count);
        }
        if let Some(last) = pieces.txb.last_mut() {
            last.count = t.idct_blocks_ptr;
            shrink(Kind::Txb, &mut pieces.piece[Kind::Txb as usize], t.idct_blocks_ptr);
        }
        let piece = &mut pieces.piece[Kind::Coefs as usize];
        if piece.1 != 0 {
            let used = t.dq_buffer_ptr - (piece.0 * COEF_CHUNK) as c_int;
            shrink(Kind::Coefs, piece, used);
        }
        // What is left of the tile's cells is nothing it can still need.
        if pieces.cells_left > 0 {
            pools.progress(slot, pieces.cells_left as usize);
            pieces.cells_left = 0;
        }
        // -memstats: what the tile wrote.
        let records = |runs: &[Run]| runs.iter().map(|run| run.count.max(0) as u64).sum::<u64>();
        super::mem_stats::tile_parsed(
            dec,
            td,
            records(&pieces.mode_info),
            (pieces.coef_values + i64::from(t.dq_buffer_ptr - pieces.coef_run)).max(0) as u64,
            records(&pieces.txb),
            t.gen_index_ptr.max(0) as u64,
        );
    }
}

/// The runs of records of `runs` (a tile's pieces in the order they were written), with the pieces that
/// follow each other as one run, and without the empty ones.
pub(crate) fn joined_runs(runs: &[Run]) -> Vec<Run> {
    let mut joined: Vec<Run> = Vec::with_capacity(runs.len());
    for run in runs.iter().filter(|run| run.count > 0) {
        match joined.last_mut() {
            Some(last) if last.first + last.count == run.first => last.count += run.count,
            _ => joined.push(*run),
        }
    }
    joined
}

/// The pixels of one plane of a palette block, written into the slot's palette buffer.
///
/// # Safety
/// `dst` is valid for `height` rows of `stride` samples from where the block starts; the map has
/// `map.rows` rows of `width` entries, `map.stride` apart, each an index of the 8 colours from `colors`.
unsafe fn write_palette_plane<T: Copy>(
    mut dst: *mut T,
    stride: usize,
    map: &PaletteMap,
    width: usize,
    height: usize,
    colors: &[u16],
    sample: impl Fn(u16) -> T,
) {
    for row in 0..height {
        // The rows of the block behind the last one of the map repeat it.
        // SAFETY: a row of the map (the caller's contract).
        let indexes = unsafe { map.data.add(row.min(map.rows - 1) * map.stride) };
        for j in 0..width {
            // SAFETY: the caller's contract.
            unsafe { dst.add(j).write(sample(colors[usize::from(*indexes.add(j))])) };
        }
        // SAFETY: the next row of the block.
        unsafe { dst = dst.add(stride) };
    }
}

/// The colour index map of a palette block's plane.
pub(crate) struct PaletteMap {
    pub data: *const u8,
    /// Entries from one row to the next.
    pub stride: usize,
    /// The rows the map has; a block that is higher repeats the last one.
    pub rows: usize,
}

/// The pixels of a palette block go into the palette buffer of the frame's slot, in the layout of the
/// frame buffer: `plane` 0 is luma, 1 both chroma planes from the one colour map. `colors` has eight
/// colours per plane (Y, U, V); the block is `width` x `height` samples of the plane.
///
/// # Safety
/// `dec` is the decoder while the tiles of the frame on the slot `td` are decoded; `map` is valid for
/// `width` entries a row.
#[allow(clippy::too_many_arguments)]
pub(crate) unsafe fn write_palette(
    _dec: *mut Av1Core,
    td: *const av1_frame_thread_data,
    mi_row: c_int,
    mi_col: c_int,
    plane: usize,
    colors: &[u16; 24],
    map: &PaletteMap,
    width: usize,
    height: usize,
) {
    // SAFETY: the caller's contract.
    unsafe {
        let td = &*td;
        // Palette blocks exist only in frames that allow screen content tools, and the frame setup has
        // created the buffer for those.
        if td.palette_buffer.is_null() {
            return;
        }
        // The layout of the frame: its frame buffer, or the back buffer it is reconstructed into, may
        // not be the frame's yet while its tiles are parsed.
        let fb = &td.layout;
        super::mem_stats::palette_block(
            _dec,
            td,
            width * height * if fb.hbd != 0 { 2 } else { 1 } * if plane != 0 { 2 } else { 1 },
        );
        let bsz = 4 >> plane;
        let palette_buf = (*td.palette_buffer).host_ptr.cast::<u8>();
        let plane_start = |p: usize| palette_buf.offset(fb.planes[p].offset as isize - fb.base_offset as isize);
        if fb.hbd != 0 {
            let stride = (fb.planes[plane].stride >> 1) as usize;
            let offset = ((mi_row * bsz) & !3) as usize * stride + ((mi_col * bsz) & !3) as usize;
            let dst = plane_start(plane).cast::<u16>().add(offset);
            write_palette_plane(dst, stride, map, width, height, &colors[8 * plane..], |c| c);
            if plane != 0 {
                let dst = plane_start(2).cast::<u16>().add(offset);
                write_palette_plane(dst, stride, map, width, height, &colors[16..], |c| c);
            }
        } else {
            let stride = fb.planes[plane].stride as usize;
            let offset = ((mi_row * bsz) & !3) as usize * stride + ((mi_col * bsz) & !3) as usize;
            let dst = plane_start(plane).add(offset);
            write_palette_plane(dst, stride, map, width, height, &colors[8 * plane..], |c| c as u8);
            if plane != 0 {
                let dst = plane_start(2).add(offset);
                write_palette_plane(dst, stride, map, width, height, &colors[16..], |c| c as u8);
            }
        }
    }
}

// ---------------------------------------------------------------------------------------------------------
// The recording and the submission of a frame.
// ---------------------------------------------------------------------------------------------------------

/// `av1_decode_frame_gpu` (`dx/av1_core.h`), its first part: the stages of the frame are recorded up to
/// the output, and the application is asked for the output image of a frame that is output. Returns the
/// state of that image: 0 the frame is not output, 1 `fb` is its image, 2 the application gave none: the
/// caller raises the decoder's error. `perf_lap_start` gets the start of the timer of the output copy.
///
/// # Safety
/// `pbi` is the entropy decoder's state with a frame whose tiles are decoded; `fb` and `perf_lap_start`
/// are valid for writing; decode thread.
pub(crate) unsafe extern "C" fn rav1g_decode_frame_record(
    pbi: *mut AV1Decoder,
    fb: *mut av1_decoded_frame_buffer_t,
    perf_lap_start: *mut i64,
) -> c_int {
    // SAFETY: the caller's contract.
    unsafe { decode_frame_record(pbi, fb, perf_lap_start, None) }
}

/// [`rav1g_decode_frame_record`]. `output`: whether the frame, if it is shown, is the one of its
/// temporal unit that is output, for a caller that asked when the frame began (the question looks
/// through the data of the decode call, which a frame that is recorded later no longer has); `None`
/// asks here.
///
/// # Safety
/// As for [`rav1g_decode_frame_record`]; the thread that records the frames.
pub(crate) unsafe fn decode_frame_record(
    pbi: *mut AV1Decoder,
    fb: *mut av1_decoded_frame_buffer_t,
    perf_lap_start: *mut i64,
    output: Option<bool>,
) -> c_int {
    // SAFETY: the caller's contract.
    unsafe {
        let dec = (*pbi).gpu_decoder.cast::<Av1Core>();
        let cm = &raw mut (*pbi).common;
        let td = (*dec).curr_frame_data;
        (*td).fused_sp_active = 0;
        (*td).fused_sp_luma_done = 0;
        (*td).fused_sp_chroma_done = 0;
        (*td).cdef_lists_front = 0;
        (*td).output_copy_reason = sys::AV1_PERF_OUTPUT_COPY_OTHER;
        (*td).perf.output_path = sys::AV1_PERF_OUTPUT_NONE;
        (*cm).mi = (*(*td).parse_buffers[Kind::ModeInfo as usize]).host_ptr.cast();
        let mut perf_t = super::perf::av1_perf_ticks();
        prepare_command_buffer(dec);
        stage_dump_frame_info(dec, cm);
        let lf_lists = record_front(pbi, dec);
        perf_t = perf_lap(dec, sys::AV1_PERF_CPU_REC_GEN_BLOCKS as usize, perf_t);
        idct_run(dec);
        perf_t = perf_lap(dec, sys::AV1_PERF_CPU_REC_IDCT as usize, perf_t);
        prediction_run_all(dec, cm);
        perf_t = perf_lap(dec, sys::AV1_PERF_CPU_REC_PRED as usize, perf_t);
        loopfilter_gpu(dec, cm, lf_lists);
        perf_t = perf_lap(dec, sys::AV1_PERF_CPU_REC_LOOPFILTER as usize, perf_t);

        // Request the output image before postfiltering so direct NV12/P010 output can be fused into
        // the last postfilter stage (CDEF v3, superres, or loop restoration).
        let is_output = (*cm).show_frame != 0 && output.unwrap_or_else(|| super::obu::frame_is_output(pbi));
        (*(*dec).curr_frame_data).output_frame = c_int::from(is_output);
        let image_state = output_image(dec, cm, fb);
        let fused = if image_state == 1 { prepare_fused_output(dec, cm, fb) } else { None };
        perf_t = perf_lap(dec, sys::AV1_PERF_CPU_REC_COPY as usize, perf_t);

        cdef_looprestoration(dec, cm, (&raw mut (*pbi).lr_ctxt).cast(), fused);
        perf_t = perf_lap(dec, sys::AV1_PERF_CPU_REC_CDEF_LR as usize, perf_t);
        // The borders are for inter prediction of later frames: none reads those of a frame that no slot of
        // the reference map takes (AV1 7.20).
        if !frame_unreferenced(dec, cm) {
            inter_ext_borders(dec, cm);
        }
        perf_t = perf_lap(dec, sys::AV1_PERF_CPU_REC_BORDERS as usize, perf_t);
        *perf_lap_start = perf_t;
        image_state
    }
}

/// `av1_decode_frame_gpu`, its second part: the copy into the output image is recorded, the command list
/// is submitted, and the frame goes to the completion thread with its slot and its image.
///
/// # Safety
/// As for [`rav1g_decode_frame_record`], after it, with the state and the lap start it returned.
pub(crate) unsafe extern "C" fn rav1g_decode_frame_submit(
    pbi: *mut AV1Decoder,
    image_state: c_int,
    fb: *const av1_decoded_frame_buffer_t,
    perf_lap_start: i64,
) {
    // SAFETY: the caller's contract.
    unsafe {
        let dec = (*pbi).gpu_decoder.cast::<Av1Core>();
        let cm = &raw mut (*pbi).common;
        if image_state != 0 {
            copy_output(dec, cm, fb);
        }
        super::mem_stats::frame_recorded(dec, &*cm);
        let mut perf_t = perf_lap(dec, sys::AV1_PERF_CPU_REC_COPY as usize, perf_lap_start);
        if perf_t != 0 {
            let td = &*(*dec).curr_frame_data;
            let acc = &mut *super::perf::frame_acc(dec);
            acc.frame_number = td.frame_number;
            acc.show_frame = (*cm).show_frame;
            acc.showable_frame = (*cm).showable_frame;
            let frame_type = (*cm).current_frame.frame_type as c_int;
            acc.intra_frame = c_int::from(frame_type == 0 || frame_type == 2); // frame_is_intra_only()
            acc.intra_iters = td.intra_iters;
            acc.intra_stats = td.intra_stats;
            acc.tile_count = td.tile_count;
        }
        commit_command_buffer(dec);
        perf_t = super::perf::av1_perf_ticks();
        let item: *mut GpuWorkItem = (*dec).gpu_item_pool.get();
        perf_add(dec, sys::AV1_PERF_CPU_WAIT_ITEM as usize, perf_t);
        (*item).data = (*dec).curr_frame_data;
        (*item).image = std::ptr::null_mut();
        // The perf record's key to the application's own log (av1_perf_frame::user_data).
        (*(*dec).curr_frame_data).perf.acc.user_data = (*pbi).user_priv as usize as u64;
        (*dec).curr_frame_data = std::ptr::null_mut();
        let buf = &mut (*(*cm).cur_frame).buf;
        let image: *mut HwOutputImage = buf.hw_show_image;
        if !image.is_null() {
            // The frame is output: the output copy has put its image here.
            (*item).image = image;
            (*image).user_priv = (*pbi).user_priv;
            buf.hw_show_image = std::ptr::null_mut();
        }
        (*dec).gpu_waiting_queue.push(item);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_layout_of_a_frame_moves_with_its_buffer() {
        // SAFETY: numbers; all zero is a value.
        let mut first: HwFrameBuffer = unsafe { std::mem::zeroed() };
        let mut second = first;
        second.base_offset = 4096;
        // 352x288 10-bit: rows of 384 + 32 samples, 384 rows of luma.
        layout_frame_buffer(&mut first, 352, 288, 1);
        layout_frame_buffer(&mut second, 352, 288, 1);
        assert_eq!((first.width, first.height, first.hbd), (352, 288, 1));
        assert_eq!(first.planes.map(|plane| plane.stride), [832, 448, 448]);
        assert_eq!(first.planes.map(|plane| plane.offset), [32, 319_520, 405_536]);
        for (a, b) in first.planes.iter().zip(&second.planes) {
            assert_eq!((b.offset - a.offset, b.stride, b.res_offset), (4096, a.stride, a.res_offset));
        }
        // The description of the frame without a buffer says the same sizes.
        // SAFETY: numbers and pointers; all zero is a value.
        let mut ybf: YV12_BUFFER_CONFIG = unsafe { std::mem::zeroed() };
        describe_frame(&mut ybf, 352, 288, 1);
        assert_eq!((ybf.crop_widths[0], ybf.crop_heights[0], ybf.strides, ybf.flags), (352, 288, [416, 224], 8));
        assert_eq!(ybf.frame_size, 2 * (384 * 416 + 2 * 192 * 224));
        assert!(ybf.hw_buffer.is_null());
    }

    #[test]
    fn a_scaled_reference() {
        // SAFETY: numbers and function pointers; all zero is a value.
        let mut sf: scale_factors = unsafe { std::mem::zeroed() };
        sf.x_scale_fp = REF_NO_SCALE as c_int;
        sf.y_scale_fp = REF_NO_SCALE as c_int;
        assert!(!is_scaled(&sf));
        sf.x_scale_fp = 2 * REF_NO_SCALE as c_int;
        assert!(is_scaled(&sf));
        sf.y_scale_fp = REF_INVALID_SCALE;
        assert!(!is_scaled(&sf));
    }

    /// The constants that are expressions or of other headers than the bindings read.
    #[test]
    fn constants_are_those_of_the_headers() {
        let read = |path: &str| {
            std::fs::read_to_string(format!("{}/../../libav1/{path}", env!("CARGO_MANIFEST_DIR"))).expect(path)
        };
        assert!(read("aom/aom_decoder.h").contains("#define AV1_MEMORY_GUARD_MARGIN_BUFFER ((size_t)16 << 20)"));
        assert_eq!(MEMORY_GUARD_MARGIN_BUFFER, 16 << 20);
        assert!(read("av1/common/scale.h").contains("#define REF_INVALID_SCALE -1"));
        assert!(read("av1/common/blockd.h").contains("#define MAX_MB_PLANE 3"));
        assert!(read("aom_scale/yv12config.h").contains("#define YV12_FLAG_HIGHBITDEPTH 8"));
        assert!(read("av1/common/mv.h").contains("  TRANSLATION = 1,"));
        assert_eq!(TRANSLATION, 1);
    }
}
