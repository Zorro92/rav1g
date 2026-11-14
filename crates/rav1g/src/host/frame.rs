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
use std::sync::atomic::{AtomicI32, Ordering};

use windows::Win32::Graphics::Direct3D12::{ID3D12CommandQueue, ID3D12Device};

use super::blocks::WaveEdges;
use super::completion::start_completion_thread;
use super::core::{buffer, commands, pipeline};
use super::create::{IDCT_COEF_COUNT_DENUM, to_mib};
use super::log::log;
use super::loopfilter::loopfilter_gpu;
use super::memory::poison_constants;
use super::output::{copy_output, output_image};
use super::perf::{
    inter_fast_mode, perf_add, perf_add_memset, perf_begin_command_buffer, perf_detail_mode, perf_gpu_mark, perf_lap,
    perf_now, perf_resolve,
};
use super::postfilter::cdef_looprestoration;
use super::prediction::{inter_ext_borders, prediction_gen_blocks, prediction_run_all};
use super::stage_dump::{stage_dump_begin, stage_dump_frame_info};
use super::thread::lock;
use super::transform::idct_run;
use super::types::{
    Av1Core, DeviceUpload, EntropyThreadCount, GpuWorkItem, InterFastMapSize, IntraV2MaxRegions, av1_frame_thread_data,
};
use crate::gpu::d3d12::{com_ref, wrapped_ref};
use crate::gpu::{BufferState, CommandAllocator, CommandList, Event, Fence, Queue};
use crate::sys;
use crate::sys::parser::{
    ALTREF_FRAME, AV1_COMMON, AV1Decoder, Av1ColorMapParam, HwFrameBuffer, HwOutputImage, INVALID_IDX, LAST_FRAME,
    MACROBLOCKD, MB_MODE_INFO, REF_NO_SCALE, RESTORE_NONE, ThreadData, TileInfo, YV12_BUFFER_CONFIG,
    av1_decoded_frame_buffer_t, av1_tile_data, scale_factors,
};

/// A tile of at most this many superblocks is too small for the switch to a second share of the
/// coefficient buffer at a superblock boundary (`TileSbSizeThreshold`).
const TILE_SB_SIZE_THRESHOLD: c_int = 2;
/// `AV1_MEMORY_GUARD_MARGIN_BUFFER` of `aom/aom_decoder.h`: what a buffer that is created after the
/// decoder must leave of the application's memory.
const MEMORY_GUARD_MARGIN_BUFFER: u64 = 16 << 20;
/// `MAX_MB_PLANE` of `av1/common/blockd.h`.
const MAX_MB_PLANE: c_int = 3;
/// `REF_INVALID_SCALE` of `av1/common/scale.h`.
const REF_INVALID_SCALE: c_int = -1;
/// `TRANSLATION` of `av1/common/mv.h`: the last global motion type that is not a warp.
const TRANSLATION: c_int = 1;
/// `YV12_FLAG_HIGHBITDEPTH` of `aom_scale/yv12config.h`.
const YV12_FLAG_HIGHBITDEPTH: c_int = 8;

unsafe extern "C" {
    /// `av1/decoder/obu.c`: the frame is the one of its temporal unit that is output.
    fn av1_frame_is_output(pbi: *const AV1Decoder) -> c_int;
}

const CLEARS_NOT_NEEDED: c_int = sys::AV1_CLEARS_NOT_NEEDED as c_int;
const CLEARS_GPU: c_int = sys::AV1_CLEARS_GPU as c_int;
const CLEARS_COMPLETION_THREAD: c_int = sys::AV1_CLEARS_COMPLETION_THREAD as c_int;
const CLEARS_DECODE_THREAD: c_int = sys::AV1_CLEARS_DECODE_THREAD as c_int;

/// What a frame does about the clears of its slot's grids (`AV1_CLEARS_*` of `dx/av1_perf.h`). Without a
/// call of `av1_set_clears` nothing of the wavefront grid is cleared: on the console the processor has a
/// fraction of its memory bandwidth for most of a playback, the clears were 3.5 MB a 4K frame that the
/// parse competed with, and the GPU has time to spare (docs/phase2-rav1d.md, "No clears by the CPU").
/// See [`rav1g_setup_frame_begin`] for the rules.
static CLEARS: AtomicI32 = AtomicI32::new(CLEARS_NOT_NEEDED);

/// `av1_set_clears` (`dx/av1_perf.h`). A mode the header does not have is the default.
#[unsafe(no_mangle)]
pub extern "C" fn av1_set_clears(mode: c_int) {
    let known = (CLEARS_NOT_NEEDED..=CLEARS_DECODE_THREAD).contains(&mode);
    CLEARS.store(if known { mode } else { CLEARS_NOT_NEEDED }, Ordering::Relaxed);
}

/// `av1_set_async_clear` (`dx/av1_perf.h`): the older form of the switch.
#[unsafe(no_mangle)]
pub extern "C" fn av1_set_async_clear(enable: c_int) {
    av1_set_clears(if enable != 0 { CLEARS_NOT_NEEDED } else { CLEARS_DECODE_THREAD });
}

/// The switch as it is now.
pub(crate) fn clears_mode() -> c_int {
    CLEARS.load(Ordering::Relaxed)
}

/// `av1_shader_variant` (`dx/av1_perf.h`): "uavsync" for the build whose shaders have their group syncs
/// as syncs of all memory (the feature `uavsync`), else an empty string.
#[unsafe(no_mangle)]
pub extern "C" fn av1_shader_variant() -> *const std::ffi::c_char {
    if cfg!(feature = "uavsync") { c"uavsync".as_ptr() } else { c"".as_ptr() }
}

/// Test player switch (-intrav2): intra prediction through intra_v2.hlsl, which writes the same bytes
/// with one thread group per 64x64 region and far fewer dispatches. Off unless `av1_set_intra_v2(1)` was
/// called.
static INTRA_V2: AtomicI32 = AtomicI32::new(0);

/// `av1_set_intra_v2` (`dx/av1_perf.h`).
#[unsafe(no_mangle)]
pub extern "C" fn av1_set_intra_v2(enable: c_int) {
    INTRA_V2.store(enable, Ordering::Relaxed);
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
// What the entropy decoder asks for outside a frame's setup.
// ---------------------------------------------------------------------------------------------------------

/// `av1_setup_context_buffers` (`dx/av1_core.h`): the context arrays of the entropy decoder are the host
/// memory the decoder set aside for them at its creation.
///
/// # Safety
/// `pbi` is the entropy decoder's state of a decoder.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn av1_setup_context_buffers(pbi: *mut AV1Decoder) {
    // SAFETY: the caller's contract.
    unsafe {
        let cm = &mut (*pbi).common;
        let dec = &*(*pbi).gpu_decoder.cast::<Av1Core>();
        // The above context.
        let cols = (cm.mi_cols + 31) & !31;
        let rows = cm.tile_rows;
        cm.num_allocated_above_contexts = rows;
        cm.num_allocated_above_context_mi_col = cols;
        cm.num_allocated_above_context_planes = if cm.seq_params.monochrome != 0 { 1 } else { MAX_MB_PLANE };
        cm.above_context[0] = dec.above_context_alloc[0].cast();
        cm.above_context[1] = dec.above_context_alloc[1].cast();
        cm.above_context[2] = dec.above_context_alloc[2].cast();
        cm.above_seg_context = dec.above_context_alloc[3].cast();
        cm.above_txfm_context = dec.above_context_alloc[4].cast();

        cm.rst_info[0].unit_info = dec.restoration_info_alloc[0].cast();
        cm.rst_info[1].unit_info = dec.restoration_info_alloc[1].cast();
        cm.rst_info[2].unit_info = dec.restoration_info_alloc[2].cast();
        cm.tpl_mvs = dec.tplmvs_alloc.cast();
    }
}

/// `av1_show_frame` (`dx/av1_core.h`): nothing; a frame is output by its command list.
#[unsafe(no_mangle)]
pub extern "C" fn av1_show_frame(_pbi: *mut AV1Decoder, _buf: *mut YV12_BUFFER_CONFIG, _is_visible: c_int) {}

/// `av1_sequence_fits_decoder` (`dx/av1_core.h`): whether the frames of a sequence fit the buffers the
/// decoder was created with: its largest frame, rounded up to 128 as the buffers are, and its bit depth
/// (frame buffers of one byte a sample hold 8-bit frames only; those of two bytes hold both). The sizes
/// the decoder has go to `dec_*` for the message.
///
/// # Safety
/// `dec` is a decoder; the three out pointers are null or valid for writing.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn av1_sequence_fits_decoder(
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

/// `av1_reallocate_frame_buffer` (`dx/av1_core.h`): the frame buffer of the frame whose header is being
/// read: a buffer of the pool (the call waits for one when the pool is empty) with the layout of a frame
/// of this size. Returns -1 for a frame the decoder's buffers were not created for.
///
/// # Safety
/// `private` is null or the decoder; `ybf` is null or the buffer description of the frame.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn av1_reallocate_frame_buffer(
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
        let bpp = hbd + 1;
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
        let superres_size = (y_height * (superres_w + 2 * h_border)
            + 2 * (y_height >> 1) * ((superres_w >> 1) + 2 * h_border))
            * (1 + hbd);
        // The frame buffer is not the only buffer sized at creation: the per-column and per-row tables, the
        // block lists and the grids are sized for the width and the height the decoder was created with. A
        // frame that is wider or higher than that is refused even when its bytes would fit a frame buffer.
        let size = (*ybf.hw_buffer).size;
        if size < fb_size as usize
            || size < superres_size as usize
            || aligned_width > (*dec).max_width
            || superres_w > (*dec).max_width
            || aligned_height > (*dec).max_height
            || (width != upscaled_width && (*dec).enable_superres == 0)
        {
            (*dec).fb_pool.release(ybf.hw_buffer);
            ybf.hw_buffer = std::ptr::null_mut();
            return -1;
        }

        ybf.buffer_alloc = std::ptr::null_mut();
        ybf.buffer_alloc_sz = 0;
        ybf.__bindgen_anon_3.crop_widths[0] = width; // y_crop_width
        ybf.__bindgen_anon_4.crop_heights[0] = height; // y_crop_height
        let y_width = (width + 7) & !7;
        let y_height_visible = (height + 7) & !7;
        ybf.__bindgen_anon_1.widths[0] = y_width;
        ybf.__bindgen_anon_2.heights[0] = y_height_visible;
        ybf.__bindgen_anon_5.strides[0] = y_stride;

        ybf.__bindgen_anon_3.crop_widths[1] = (width + 1) >> 1; // uv_crop_width
        ybf.__bindgen_anon_4.crop_heights[1] = (height + 1) >> 1; // uv_crop_height
        ybf.__bindgen_anon_1.widths[1] = y_width >> 1;
        ybf.__bindgen_anon_2.heights[1] = y_height_visible >> 1;
        ybf.__bindgen_anon_5.strides[1] = uv_stride;

        ybf.border = h_border;
        ybf.frame_size = fb_size as usize;
        ybf.subsampling_x = 1;
        ybf.subsampling_y = 1;
        ybf.use_external_reference_buffers = 0;
        ybf.flags = if hbd != 0 { YV12_FLAG_HIGHBITDEPTH } else { 0 };

        ybf.__bindgen_anon_6.buffers = [std::ptr::null_mut(); 3];
        let buf = &mut *ybf.hw_buffer;
        let uv_width = y_width >> 1;
        let uv_height = y_height_visible >> 1;
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
        let short = size_of::<i16>() as c_int;
        buf.planes[0].res_stride = short * ((y_width + 127) & !127);
        buf.planes[1].res_stride = short * ((uv_width + 127) & !127);
        buf.planes[2].res_stride = short * ((uv_width + 127) & !127);
        buf.planes[0].res_offset = 0;
        buf.planes[1].res_offset = buf.planes[0].res_stride * ((y_height_visible + 127) & !127);
        buf.planes[2].res_offset = buf.planes[1].res_offset + buf.planes[1].res_stride * ((uv_height + 127) & !127);
    }
    0
}

/// `av1_release_fb_callback` (`dx/av1_core.h`): the entropy decoder lets go of a frame: its reference to
/// the frame buffer, and an output image the frame still had goes back to the application.
///
/// # Safety
/// `private` is the decoder; `ybf` is the buffer description of a frame of it.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn av1_release_fb_callback(private: *mut c_void, ybf: *mut YV12_BUFFER_CONFIG) {
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

/// `get_sec_data` of the C++: the frame takes a second slot, for the share of that slot in the mode info
/// pool and in the coefficient buffer. Waits for a slot when none is free. Called by the decode thread and
/// by the tile threads.
///
/// # Safety
/// `dec` is the decoder and `td` the slot of the frame that is being decoded.
unsafe fn get_sec_data(dec: *mut Av1Core, td: *mut av1_frame_thread_data) {
    // SAFETY: the caller's contract; the second slot is taken under the lock of the first.
    unsafe {
        let _taking = lock(&(*td).sec_data_mutex);
        if (*td).sec_thread_data.is_null() {
            let perf_t = super::perf::av1_perf_ticks();
            // With more than one frame in parse a frame takes a slot only when the frames before it
            // can still get theirs (`rav1d_host::SlotGate`).
            super::rav1d_host::gate_second_slot(dec, td);
            (*td).sec_thread_data = (*dec).frame_data_pool.get();
            perf_add(dec, sys::AV1_PERF_CPU_WAIT_SEC_SLOT as usize, perf_t);
            if perf_t != 0 {
                (*super::perf::frame_acc(dec)).used_sec_slot = 1;
            }
            let sec = &*(*td).sec_thread_data;
            for i in 0..usize::try_from((*td).tile_count).unwrap_or(0) {
                (*sec.tile_data.add(i)).mi_count = 0;
            }
        }
    }
}

/// `intra_v2_next_stamp` of `dx/intra_v2_sched.h`: moves on to the stamp of the next frame. Returns true
/// when the cell map and the regions have to be cleared first: for the first frame of a buffer (stamp 0)
/// and whenever the 8-bit stamp starts over.
fn intra_v2_next_stamp(stamp: &mut u32) -> bool {
    *stamp = (*stamp % 255) + 1;
    *stamp == 1
}

/// Decides whether the frame takes the -intrav2 path and gets its schedule ready. Returns 0 when the
/// frame stays on the original shaders: flag off, intra block copy (not ported), or a frame the buffers
/// were not sized for.
///
/// # Safety
/// `dec` is the decoder, `cm` the frame's header and `td` its slot.
unsafe fn intra_v2_setup_frame(dec: *mut Av1Core, cm: &AV1_COMMON, td: &mut av1_frame_thread_data) -> c_int {
    // No buffers: the decoder was not created for this path (aom_codec_dec_cfg_t::intra_v2).
    if INTRA_V2.load(Ordering::Relaxed) == 0
        || cm.allow_intrabc != 0
        || td.intra_v2_buffer.is_null()
        || td.intra_v2_regions.is_null()
    {
        return 0;
    }
    let cols = (cm.mi_cols + 31) & !31; // 4x4 cells, padded to 128 pixels: blocks may reach that far
    let rows = (cm.mi_rows + 31) & !31;
    let cells = cols * rows;
    if cells + (cells >> 2) > td.intra_v2_cells
        || (cols >> 4) > IntraV2MaxRegions as c_int
        || (rows >> 4) > IntraV2MaxRegions as c_int
    {
        return 0;
    }
    let all_cells = (cells + (cells >> 2)) as usize;
    // SAFETY: the buffer and the regions of the slot, sized for at least `all_cells` cells (checked above).
    unsafe {
        let s = &mut td.intra_v2_sched;
        s.cells = (*td.intra_v2_buffer).host_ptr.cast();
        s.regions = td.intra_v2_regions;
        s.list = s.cells.add(2 * all_cells).cast();
        s.list_count = &raw mut td.intra_v2_list_count;
        s.map_stride = cols;
        s.map_uv_offset = cells;
        s.regions_stride = cols >> 4;
        // Entries of earlier frames stay in the cell map and are told apart by the stamp, so the map is only
        // cleared when the 8-bit stamp starts over (every 255 frames of this slot) or the layout changes.
        let mut clear = intra_v2_next_stamp(&mut s.stamp);
        if cols != td.intra_v2_cols || rows != td.intra_v2_rows {
            td.intra_v2_cols = cols;
            td.intra_v2_rows = rows;
            s.stamp = 1;
            clear = true;
        }
        if clear {
            let perf_t = super::perf::av1_perf_ticks();
            let bytes = all_cells * 8;
            let region_bytes = size_of::<c_int>() * 2 * (cells >> 8) as usize;
            s.cells.cast::<u8>().write_bytes(0, bytes);
            s.regions.cast::<u8>().write_bytes(0, region_bytes);
            perf_add_memset(dec, sys::AV1_PERF_CPU_MEMSET as usize, perf_t, bytes + region_bytes);
        }
    }
    td.intra_v2_list_count = 0;
    1
}

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
                 content tools: {:.1} MiB; device buffers now {:.1} MiB in {} buffers",
                td.slot_index,
                td.frame_number,
                to_mib(size),
                to_mib(total as u64),
                memory.dev_obj_count
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
        if store.above.try_reserve(more).is_err() || store.above_end.try_reserve(more).is_err() {
            return false;
        }
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
    td.wave_above = store.above.as_mut_ptr();
    td.wave_above_end = store.above_end.as_mut_ptr();
    // -poisonbuf: the edges hold what earlier frames left there, mostly plausible iterations. Filled
    // again for every frame, an entry that is read without having been written by this frame is "no
    // iteration" (the byte is negative) where the block there has one, and no decoded unit at all.
    // SAFETY: the three vectors, over the part this frame uses.
    unsafe {
        poison_constants(td.wave_tiles.cast(), size_of::<WaveEdges>() * tiles);
        poison_constants(td.wave_above.cast(), size_of::<c_int>() * columns);
        poison_constants(td.wave_above_end.cast(), size_of::<i64>() * columns);
    }
    true
}

/// `AV1_CLEARS_GPU`: the last commands of the frame's command list clear the grids of its slot, the
/// bytes its setup cleared or found clean (`clear_mi_bytes`, `clear_iter_bytes`), with fill_buffer.hlsl:
/// nothing of the frame reads them behind this point, and the list is through before the frame's fence
/// signals, which is what the slot goes back to its pool on. So the next frame of the slot finds the
/// grids as a clear of its own would leave them, and no thread of the CPU has written a byte for it.
///
/// The grids are buffers the CPU writes and the shaders read (a custom heap, cached, in the state the
/// shaders read it in); in a decoder for this mode a command list may write them as well
/// (`Av1Core::gpu_clear_grids`), in the state for it, which they are in from the first clear to the
/// last. The CPU reads and writes them again through the mapping it has had all along: on an adapter
/// with one memory for both and coherent caches (WARP; the console reports itself as one) it sees what
/// the GPU wrote; an adapter without that would need the mapping made again with a range to read.
///
/// # Safety
/// `dec` is the decoder while it records the frame in `curr_frame_data`, whose stages are all recorded.
unsafe fn record_slot_clears(dec: *mut Av1Core) {
    // SAFETY: the caller's contract.
    unsafe {
        let td = &mut *(*dec).curr_frame_data;
        if td.clears != CLEARS_GPU || td.clear_pending == 0 || td.clear_mi_bytes + td.clear_iter_bytes == 0 {
            return;
        }
        let commands = commands(dec);
        let shader = pipeline(&raw const (*(*dec).shader_lib).shader_fill_buffer);
        commands.set_layout(shader.layout());
        commands.set_pipeline(shader);
        // The mode info grid to null pointers, the wavefront grid to "no iteration" (all bits set).
        for (object, bytes, value) in
            [(td.mode_info_grid, td.clear_mi_bytes, 0), (td.gen_intra_inter_grid, td.clear_iter_bytes, -1)]
        {
            if bytes == 0 {
                continue;
            }
            let grid = buffer(object);
            // The shader writes 16 bytes a thread: the bytes of the clear, rounded up, inside the buffer
            // (whose size is a multiple of 256). What is behind the clear is not part of what the next
            // frame takes as clean.
            let words = (bytes.div_ceil(16)).min((*object).size / 16) as c_int;
            commands.transition(grid, BufferState::Read, BufferState::ReadWrite);
            commands.bind_write(0, grid.location());
            commands.set_inline_constants(1, &[words, value], 0);
            commands.dispatch(((words + 63) / 64) as u32, 1, 1);
            commands.transition(grid, BufferState::ReadWrite, BufferState::Read);
        }
        td.gpu_cleared = 1;
        perf_gpu_mark(dec, sys::AV1_PERF_GPU_SLOT_CLEAR as c_int);
    }
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
        // The coefficients of a submitted frame are zeroed by the IDCT shaders as they read them (the
        // entropy decoder writes only the non-zero ones and relies on the rest being zero). This frame never
        // reached the GPU, so its share is cleared here, and the share of its second slot, which it may
        // have written too.
        let coef_buf = (*(*dec).idct_coefs).host_ptr.cast::<i32>();
        coef_buf.add(td.coef_buffer_offset as usize).cast::<u8>().write_bytes(0, td.coef_buffer_size as usize);
        if let Some(sec) = td.sec_thread_data.as_mut() {
            coef_buf.add(sec.coef_buffer_offset as usize).cast::<u8>().write_bytes(0, sec.coef_buffer_size as usize);
            (*dec).frame_data_pool.push(std::ptr::from_mut(sec));
            td.sec_thread_data = std::ptr::null_mut();
        }
        // Its grids stay dirty and are not marked clean, so the next frame on this slot clears them itself,
        // whoever does the clears otherwise.
        td.clear_pending = 0;
        td.gpu_cleared = 0;
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
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rav1g_setup_frame_begin(
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
        td.sec_thread_data = std::ptr::null_mut();
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
        // The two grids of the slot that a frame clears (this grid and the wavefront grid below), and who
        // clears them (`AV1_CLEARS_*`, `av1_set_clears`). The clears are sized from the frame, which whoever
        // clears behind a frame cannot know of the frame that comes next. The rule: behind a frame, exactly
        // the bytes that frame's setup cleared are cleared again, by the completion thread after the
        // frame's fence or by the frame's own command list (`record_slot_clears`), and the two sizes are
        // recorded in the slot (clean_*). A frame skips a clear only when the recorded size is exactly the
        // size it would clear itself; then every byte of the grid is what the clear on this thread would
        // have left, inside the range and past it. Any other case (first use of the slot, another frame
        // size, a slot that went back without a completed frame, the clears on this thread) clears here.
        // The marks are taken off at once: from here on the grids are being written.
        //
        // Without a switch (`AV1_CLEARS_NOT_NEEDED`) the wavefront grid is not cleared by anybody: the
        // frame keeps its wavefront in the edges of its tiles (below). What then still needs a clear, the
        // mode info grid of a decoder that does not keep it, is the completion thread's.
        //
        // The mode info grid is not cleared at all where a clear would change nothing a reader sees
        // (`Av1Core::keep_mi_grid`, a decoder with rav1d's parse): the parse of a frame writes the unit of
        // every 4x4 of the frame, so what the frames before left inside the frame is overwritten before
        // anything reads it (the sink reads a unit behind its block, the block lists read units of blocks
        // that are parsed, the stages run when the parse is through; a frame whose parse fails is not
        // submitted). What a clear is still needed for is the part of the grid outside the frame, which
        // the stages may read and which has to be null: it is null when the grid was cleared once for
        // these bytes and only frames of this width, height and row length were parsed into it since. So
        // the slot remembers those (`kept_grid`), and a frame of another size clears as before. That is
        // 4.2 MB a 4K frame the CPU does not write (docs/phase2-rav1d.md).
        let mut clears = clears_mode();
        if clears == CLEARS_GPU && (*dec).gpu_clear_grids == 0 {
            // The grids of this decoder are not buffers a command list may write: it was created with
            // another mode.
            clears = CLEARS_COMPLETION_THREAD;
        }
        let cleared_behind = clears != CLEARS_DECODE_THREAD;
        let mi_grid_bytes = size_of::<*mut *mut MB_MODE_INFO>() * cm.mi_alloc_size as usize;
        let keep_grid = (*dec).keep_mi_grid != 0;
        let grid_frame = [cm.mi_cols, cm.mi_rows, cm.mi_stride];
        let kept = keep_grid && td.kept_grid == grid_frame && td.kept_grid_bytes == mi_grid_bytes;
        if !kept && (!cleared_behind || td.clean_mi_bytes != mi_grid_bytes) {
            perf_t = super::perf::av1_perf_ticks();
            cm.mi_grid_base.cast::<u8>().write_bytes(0, mi_grid_bytes);
            perf_add_memset(dec, sys::AV1_PERF_CPU_MEMSET as usize, perf_t, mi_grid_bytes);
        }
        (td.kept_grid, td.kept_grid_bytes) = if keep_grid { (grid_frame, mi_grid_bytes) } else { ([0; 3], 0) };
        let clean_iter_bytes = td.clean_iter_bytes;
        td.clean_mi_bytes = 0;
        td.clean_iter_bytes = 0;
        td.clears = clears;
        td.gpu_cleared = 0;
        // A grid that is kept is not cleared behind the frame either.
        td.clear_mi_bytes = if keep_grid { 0 } else { mi_grid_bytes };
        td.clear_iter_bytes = 0;
        td.clear_pending = 1;

        let (y_width, y_height) = (buf.__bindgen_anon_1.widths[0], buf.__bindgen_anon_2.heights[0]);
        let grid_w = (((y_width + 63) & !63) >> 2) + 2 + 128;
        let grid_h = (((y_height + 63) & !63) >> 2) + 2 + 128;
        td.iter_grid_stride = grid_w;
        td.iter_grid_stride_uv = grid_w >> 1;
        td.bitdepth = buf.bit_depth as c_int;

        td.tile_count = cm.tile_cols * cm.tile_rows;
        td.thread_count = (*pbi).max_threads.min(td.tile_count);
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

        td.gen_intra_iter_y = (*td.gen_intra_inter_grid).host_ptr.cast();
        td.iter_grid_offset_uv = grid_w * grid_h;
        td.gen_intra_iter_uv = td.gen_intra_iter_y.add(td.iter_grid_offset_uv as usize);
        td.intra_stats = [0; 4];
        td.intra_v2 = intra_v2_setup_frame(dec, cm, td);
        // With -intrav2 nothing scans the iteration grid; av1_mi_push_block() writes the one entry per
        // block that gen_pred_blocks.hlsl reads. The same without a switch for the clears: the tiles keep
        // the wavefront in their edges, the grid gets that one entry per block, and nothing of it is
        // cleared, here or behind the frame.
        td.wave_edges = c_int::from(td.intra_v2 == 0 && clears == CLEARS_NOT_NEEDED && wave_edges_for_frame(cm, td));
        if td.intra_v2 == 0 && td.wave_edges == 0 {
            // The wavefront in the grid, which av1_mi_push_block() reads back: every entry no block of
            // the frame writes has to be "no iteration".
            // The luma grid (4 bytes per entry) and the chroma grid right behind it: one range from the
            // start of the buffer. Skipped when exactly this range was cleared behind the frame before.
            let iter_grid_bytes = td.iter_grid_offset_uv as usize * 5;
            if !cleared_behind || clean_iter_bytes != iter_grid_bytes {
                perf_t = super::perf::av1_perf_ticks();
                td.gen_intra_iter_y.cast::<u8>().write_bytes(0xFF, td.iter_grid_offset_uv as usize * 4);
                td.gen_intra_iter_uv.cast::<u8>().write_bytes(0xFF, td.iter_grid_offset_uv as usize);
                perf_add_memset(dec, sys::AV1_PERF_CPU_MEMSET as usize, perf_t, iter_grid_bytes);
            }
            // With -intrav2 on, the frames that still come here (intra block copy) are the exception and
            // the next frame on the slot most likely does not clear this grid, so it is not cleared ahead
            // for it.
            if INTRA_V2.load(Ordering::Relaxed) == 0 {
                td.clear_iter_bytes = iter_grid_bytes;
            }
        }

        td.is_hbd = c_int::from(buf.bit_depth > 8);
        let lib = (*dec).shader_lib;
        td.shaders = if td.is_hbd != 0 { &raw mut (*lib).shaders_hbd } else { &raw mut (*lib).shaders_8bit };

        let fb_ptr = buf.hw_buffer;
        assert!(!fb_ptr.is_null(), "the frame has its frame buffer");
        let fb = &mut *fb_ptr;

        fb.frame_number = (*dec).frame_number;
        fb.hbd = td.is_hbd;
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
        if td.do_loop_rest != 0 || td.do_cdef != 0 || td.do_superres != 0 {
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
/// the one the stages record from, and the shared back buffers are described for it.
///
/// # Safety
/// `dec` is the decoder and `td` the slot of a frame that was set up and whose tiles are parsed; the
/// thread that records the frames, which has no other frame in `curr_frame_data`.
pub(crate) unsafe fn bind_frame_for_record(dec: *mut Av1Core, td: *mut av1_frame_thread_data) {
    // SAFETY: the caller's contract.
    unsafe {
        (*dec).curr_frame_data = td;
        if (*td).frame_buffer != (*td).dst_frame_buffer {
            describe_back_buffers(dec, &*td);
        }
    }
}

/// `av1_setup_frame`, its second part: where the coefficients of the frame start. Returns 1 when a frame
/// with very small tiles has no room for its coefficients: the caller raises the decoder's error.
///
/// # Safety
/// As for [`rav1g_setup_frame_begin`], after it.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rav1g_setup_frame_coefs(pbi: *mut AV1Decoder, cm: *mut AV1_COMMON) -> c_int {
    // SAFETY: the caller's contract.
    unsafe {
        let dec = (*pbi).gpu_decoder.cast::<Av1Core>();
        setup_frame_coefs(dec, (*dec).curr_frame_data, cm)
    }
}

/// [`rav1g_setup_frame_coefs`] for the frame on the slot `td_ptr`.
///
/// # Safety
/// `dec` is the decoder, `td_ptr` the slot [`setup_frame_begin`] returned for the frame of `cm`.
pub(crate) unsafe fn setup_frame_coefs(
    dec: *mut Av1Core,
    td_ptr: *mut av1_frame_thread_data,
    cm: *const AV1_COMMON,
) -> c_int {
    // SAFETY: the caller's contract.
    unsafe {
        let td = &mut *td_ptr;
        let cm = &*cm;
        td.ext_idct_buffer = 0;
        td.ext_coef_base = td.coef_buffer_offset;
        // Worst-case layout of the coefficients (IDCT_COEF_COUNT_DENUM per 4x4 unit, av1_setup_macroblockd)
        // for a frame with a tile of at most TILE_SB_SIZE_THRESHOLD superblocks: such a tile is too small
        // for the switch to a second share at a superblock boundary.
        let mib_log2 = cm.seq_params.mib_size_log2;
        let mut coef_end = 0;
        for r in 0..cm.tile_rows as usize {
            for c in 0..cm.tile_cols as usize {
                let superblocks = (cm.tile_col_start_sb[c + 1] - cm.tile_col_start_sb[c])
                    * (cm.tile_row_start_sb[r + 1] - cm.tile_row_start_sb[r]);
                td.ext_idct_buffer |= c_int::from(superblocks <= TILE_SB_SIZE_THRESHOLD);
                let row_start = cm.tile_row_start_sb[r] << mib_log2;
                let col_start = cm.tile_col_start_sb[c] << mib_log2;
                let rows = (((cm.tile_row_start_sb[r + 1] << mib_log2).min(cm.mi_rows) - row_start) + 15) & !15;
                let cols = (((cm.tile_col_start_sb[c + 1] << mib_log2).min(cm.mi_cols) - col_start) + 15) & !15;
                let mi_offset = row_start * ((cm.mi_cols + 15) & !15) + col_start * rows;
                coef_end = coef_end.max((mi_offset + rows * cols) * IDCT_COEF_COUNT_DENUM);
            }
        }
        if td.ext_idct_buffer != 0 {
            get_sec_data(dec, td_ptr);
            // The layout runs on from one share into the next. It starts in this slot's share, as it always
            // did, when it fits that share alone or when the second slot's share is the one behind it; when
            // the second slot's share is the one in front, it starts there (it used to start in this slot's
            // share all the same and ran over the share behind it, which is another slot's or the end of the
            // buffer).
            let share = td.coef_buffer_size / size_of::<c_int>() as c_int;
            let sec = &*td.sec_thread_data;
            let sec_offset = sec.coef_buffer_offset;
            if coef_end > share {
                if coef_end <= 2 * share && sec_offset + share == td.coef_buffer_offset {
                    td.ext_coef_base = sec_offset;
                } else if coef_end > 2 * share || td.coef_buffer_offset + share != sec_offset {
                    log(
                        true,
                        &format!(
                            "MEMORY FAILED frame {}: a tile of at most {TILE_SB_SIZE_THRESHOLD} superblocks needs \
                             the worst-case coefficient layout, {coef_end} coefficients, in two shares of {share} \
                             that follow each other; the frame has the shares of slots {} and {}",
                            td.frame_number, td.slot_index, sec.slot_index
                        ),
                    );
                    return 1;
                }
            }
        }
    }
    0
}

/// `av1_setup_frame`, its last part: the completion thread runs, and the slot knows the scale of the
/// frame's references and which of them a global motion block is warped from.
///
/// # Safety
/// As for [`rav1g_setup_frame_begin`], after it; `perf_start` is what that part returned.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rav1g_setup_frame_end(pbi: *mut AV1Decoder, cm: *mut AV1_COMMON, perf_start: i64) -> c_int {
    // SAFETY: the caller's contract.
    unsafe {
        let dec = (*pbi).gpu_decoder.cast::<Av1Core>();
        setup_frame_end(dec, (*dec).curr_frame_data, cm, perf_start)
    }
}

/// [`rav1g_setup_frame_end`] for the frame on the slot `td`.
///
/// # Safety
/// As for [`setup_frame_coefs`]; `perf_start` is what [`setup_frame_begin`] returned.
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

/// `av1_setup_ext_coef_buffer` (`dx/av1_core.h`): the tile goes on with its coefficients in the share of
/// the frame's second slot.
///
/// # Safety
/// `pbi` is the entropy decoder's state while it decodes the tile of `thread_data`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn av1_setup_ext_coef_buffer(
    pbi: *mut AV1Decoder,
    _cm: *mut AV1_COMMON,
    thread_data: *mut ThreadData,
) {
    // SAFETY: the caller's contract.
    unsafe {
        let dec = (*pbi).gpu_decoder.cast::<Av1Core>();
        second_coefs(dec, (*dec).curr_frame_data, &mut *(*thread_data).tile_data);
    }
}

/// A tile goes on with its coefficients in the share of the frame's second slot, which the frame takes
/// here if it has none (the call waits for a slot).
///
/// # Safety
/// `dec` is the decoder while the tiles of the frame on the slot `cur_td` are decoded; `tile` is a tile
/// of that frame, of the calling thread.
pub(crate) unsafe fn second_coefs(dec: *mut Av1Core, cur_td: *mut av1_frame_thread_data, tile: &mut av1_tile_data) {
    // SAFETY: the caller's contract.
    unsafe {
        get_sec_data(dec, cur_td);
        let td = &*(*cur_td).sec_thread_data;
        // The tile's part of the second slot's share: the same place and size as in the first.
        tile.dq_buffer_ptr = td.coef_buffer_offset + tile.dq_buffer_offset;
        tile.dq_buffer_end = tile.dq_buffer_ptr + tile.dq_buffer_size;
    }
}

/// `av1_setup_sec_data` (`dx/av1_core.h`): the tile goes on with its mode info in the share of the
/// frame's second slot.
///
/// # Safety
/// As for [`av1_setup_ext_coef_buffer`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn av1_setup_sec_data(pbi: *mut AV1Decoder, _cm: *mut AV1_COMMON, thread_data: *mut ThreadData) {
    // SAFETY: the caller's contract.
    unsafe {
        let thread_data = &mut *thread_data;
        let dec = (*pbi).gpu_decoder.cast::<Av1Core>();
        let (tile_data2, mi_pool2) =
            second_mode_info(dec, (*dec).curr_frame_data, thread_data.tile_id as usize, &*thread_data.tile_data);
        thread_data.mi_count2 = 0;
        thread_data.mi_pool2 = mi_pool2;
        thread_data.tile_data2 = tile_data2;
    }
}

/// A tile goes on with its mode info in the share of the frame's second slot, which the frame takes here
/// if it has none (the call waits for a slot): the tile's cursors there, and where its records go on.
///
/// # Safety
/// `dec` is the decoder while the tiles of the frame on the slot `cur_td` are decoded; `prev_t` is the
/// tile `tile_id` of that frame, of the calling thread.
pub(crate) unsafe fn second_mode_info(
    dec: *mut Av1Core,
    cur_td: *mut av1_frame_thread_data,
    tile_id: usize,
    prev_t: &av1_tile_data,
) -> (*mut av1_tile_data, *mut MB_MODE_INFO) {
    // SAFETY: the caller's contract.
    unsafe {
        get_sec_data(dec, cur_td);

        let td = &*(*cur_td).sec_thread_data;
        let t = &mut *td.tile_data.add(tile_id);
        t.mi_count = 0;
        t.mi_offset = prev_t.mi_offset + td.mode_info_offset - (*cur_td).mode_info_offset;
        (t, (*(*dec).mode_info_pool).host_ptr.cast::<MB_MODE_INFO>().offset(t.mi_offset as isize))
    }
}

/// `av1_setup_macroblockd` (`dx/av1_core.h`): a tile thread starts a tile: the tile's cursors into the
/// buffers of the frame's slot (coefficients, transform block records, block index list, block maps,
/// mode info).
///
/// # Safety
/// `pbi` is the entropy decoder's state while it decodes the frame of `cm`; `thread_data` is the calling
/// tile thread's and `tile` its tile.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn av1_setup_macroblockd(
    pbi: *mut AV1Decoder,
    cm: *mut AV1_COMMON,
    thread_data: *mut ThreadData,
    tile: *mut TileInfo,
) {
    // SAFETY: the caller's contract.
    unsafe {
        let thread_data = &mut *thread_data;
        let xd: *mut MACROBLOCKD = &raw mut thread_data.xd;
        let dec = (*pbi).gpu_decoder.cast::<Av1Core>();
        let setup = setup_tile(dec, (*dec).curr_frame_data, &*cm, &*tile);
        thread_data.tile_data = setup.tile_data;
        (*xd).tile_data = setup.tile_data;
        thread_data.tile_id = setup.tile_id;
        thread_data.mi_count = 0;
        thread_data.mi_count_max = setup.mi_count_max;
        thread_data.mi_pool = setup.mi_pool;
        thread_data.mi_count2 = 0;
        thread_data.mi_pool2 = std::ptr::null_mut();
        thread_data.tile_data2 = std::ptr::null_mut();
        thread_data.ext_idct_buffer = setup.ext_idct_buffer;
    }
}

/// What a tile thread starts a tile with ([`setup_tile`]).
pub(crate) struct TileSetup {
    /// The tile's cursors into the buffers of the frame's slot.
    pub tile_data: *mut av1_tile_data,
    pub tile_id: c_int,
    /// The tile's part of the slot's share of the mode info pool, and the records it has room for.
    pub mi_pool: *mut MB_MODE_INFO,
    pub mi_count_max: c_int,
    /// The frame's coefficients have the worst-case layout: no tile goes on in a second share.
    pub ext_idct_buffer: c_int,
}

/// A tile of the frame on the slot `td` starts: its cursors into the buffers of the frame's slot
/// (coefficients, transform block records, block index list, block maps, mode info).
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
        let tile_rows = (tile.mi_row_end - tile.mi_row_start + 15) & !15;
        let tile_cols = (tile.mi_col_end - tile.mi_col_start + 15) & !15;
        let mi_offset = tile.mi_row_start * ((cm.mi_cols + 15) & !15) + tile.mi_col_start * tile_rows;
        let mi_max = tile_rows * tile_cols;

        let coef_num = (*dec).coef_count_num;
        let per_unit = if td.ext_idct_buffer != 0 { IDCT_COEF_COUNT_DENUM } else { coef_num };
        t.dq_buffer_offset = mi_offset * per_unit;
        t.dq_buffer_ptr =
            (if td.ext_idct_buffer != 0 { td.ext_coef_base } else { td.coef_buffer_offset }) + t.dq_buffer_offset;
        t.dq_buffer_base = (*(*dec).idct_coefs).host_ptr.cast();
        let mib_sz = cm.seq_params.mib_size;
        let sb_sz = mib_sz * mib_sz * IDCT_COEF_COUNT_DENUM;
        t.dq_buffer_max = t.dq_buffer_ptr + mi_max * coef_num - sb_sz;
        // The tile's part of a share, and where the part it is writing ends: av1_read_coeffs_txb() does not
        // write behind it. With the worst-case layout the part is as large as the tile can need.
        t.dq_buffer_size = mi_max * coef_num;
        t.dq_buffer_end = t.dq_buffer_ptr + mi_max * per_unit;

        let blocks_4x4_count = (mi_offset * 3 + 1) >> 1;
        t.intra_iter_max = -1;
        t.intra_iter_max_uv = -1;

        t.blocks_offset = blocks_4x4_count;
        t.idct_blocks_host = (*td.idct_blocks_unordered)
            .host_ptr
            .cast::<crate::sys::parser::tx_block_info_gpu>()
            .offset(blocks_4x4_count as isize);

        let mi_offset_base =
            tile.mi_row_start * cm.mi_stride + tile.mi_col_start * (tile.mi_row_end - tile.mi_row_start);
        let mi_max_base = (tile.mi_row_end - tile.mi_row_start) * (tile.mi_col_end - tile.mi_col_start);
        // The tile's part of the slot's share of the mode info pool: half of its 4x4 units, a quarter with
        // AV1_LOW_MEMORY_MODE_INFO. The part in a second slot's share has the same place and size.
        t.mi_offset = (mi_offset_base >> (*dec).mode_info_shift) + td.mode_info_offset;
        t.mi_count = 0;
        let setup = TileSetup {
            tile_data: t_ptr,
            tile_id,
            mi_count_max: mi_max_base >> (*dec).mode_info_shift,
            mi_pool: (*(*dec).mode_info_pool).host_ptr.cast::<MB_MODE_INFO>().offset(t.mi_offset as isize),
            ext_idct_buffer: td.ext_idct_buffer,
        };

        t.mi_col_start = tile.mi_col_start;
        t.mi_row_start = tile.mi_row_start;
        t.gen_index_ptr = 0;
        t.gen_index_base = blocks_4x4_count * 2;
        t.gen_indexes = (*td.gen_mi_block_indexes).host_ptr.cast::<u32>().offset(t.gen_index_base as isize);
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
                plane.above = td.wave_above.add(first + at);
                plane.above_end = td.wave_above_end.add(first + at);
                plane.stamp = td.wave_stamp;
            }
            t.wave_edges = std::ptr::from_mut(edges).cast();
        }
        setup
    }
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

/// `av1_intra_palette` (`dx/av1_core.h`): the pixels of a palette block go into the palette buffer of the
/// frame's slot, in the layout of the frame buffer; for chroma both planes from the one colour map.
///
/// # Safety
/// `pbi` is the entropy decoder's state while it decodes a frame; `mi` is the block and `params` its
/// colour map for `plane`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn av1_intra_palette(
    pbi: *mut AV1Decoder,
    mi: *mut MB_MODE_INFO,
    params: *mut Av1ColorMapParam,
    plane: c_int,
) {
    // SAFETY: the caller's contract.
    unsafe {
        let dec = (*pbi).gpu_decoder.cast::<Av1Core>();
        let mi = &*mi;
        let params = &*params;
        let (width, height) = (params.plane_width as usize, params.plane_height as usize);
        let map = PaletteMap { data: params.color_map, stride: width, rows: height };
        let colors = &mi.palette_mode_info.palette_colors;
        write_palette(dec, (*dec).curr_frame_data, mi.mi_row, mi.mi_col, plane as usize, colors, &map, width, height);
    }
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
        let fb = &*td.frame_buffer;
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
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rav1g_decode_frame_record(
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
        let mut perf_t = super::perf::av1_perf_ticks();
        prepare_command_buffer(dec);
        stage_dump_frame_info(dec, cm);
        prediction_gen_blocks(pbi, dec);
        perf_t = perf_lap(dec, sys::AV1_PERF_CPU_REC_GEN_BLOCKS as usize, perf_t);
        idct_run(dec);
        perf_t = perf_lap(dec, sys::AV1_PERF_CPU_REC_IDCT as usize, perf_t);
        prediction_run_all(dec, cm, std::ptr::null_mut());
        perf_t = perf_lap(dec, sys::AV1_PERF_CPU_REC_PRED as usize, perf_t);
        loopfilter_gpu(dec, cm, &raw mut (*pbi).mb);
        perf_t = perf_lap(dec, sys::AV1_PERF_CPU_REC_LOOPFILTER as usize, perf_t);
        cdef_looprestoration(dec, cm, (&raw mut (*pbi).lr_ctxt).cast());
        perf_t = perf_lap(dec, sys::AV1_PERF_CPU_REC_CDEF_LR as usize, perf_t);
        inter_ext_borders(dec, cm);
        perf_t = perf_lap(dec, sys::AV1_PERF_CPU_REC_BORDERS as usize, perf_t);
        *perf_lap_start = perf_t;
        // A shown frame is output, unless it is a lower spatial layer of its temporal unit.
        let is_output = (*cm).show_frame != 0 && output.unwrap_or_else(|| av1_frame_is_output(pbi) != 0);
        (*(*dec).curr_frame_data).output_frame = c_int::from(is_output);
        output_image(dec, cm, fb)
    }
}

/// `av1_decode_frame_gpu`, its second part: the copy into the output image is recorded, the command list
/// is submitted, and the frame goes to the completion thread with its slot and its image.
///
/// # Safety
/// As for [`rav1g_decode_frame_record`], after it, with the state and the lap start it returned.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rav1g_decode_frame_submit(
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
        // The clears of the slot by the GPU, behind everything that reads its grids.
        record_slot_clears(dec);
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
            acc.intra_v2 = td.intra_v2;
            acc.intra_stats = td.intra_stats;
            acc.tile_count = td.tile_count;
        }
        commit_command_buffer(dec);
        perf_t = super::perf::av1_perf_ticks();
        let item: *mut GpuWorkItem = (*dec).gpu_item_pool.get();
        perf_add(dec, sys::AV1_PERF_CPU_WAIT_ITEM as usize, perf_t);
        (*item).data = (*dec).curr_frame_data;
        (*item).image = std::ptr::null_mut();
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
    fn the_stamp_of_the_intra_schedule() {
        let mut stamp = 0;
        assert!(intra_v2_next_stamp(&mut stamp));
        assert_eq!(stamp, 1);
        for expected in 2..=255 {
            assert!(!intra_v2_next_stamp(&mut stamp));
            assert_eq!(stamp, expected);
        }
        assert!(intra_v2_next_stamp(&mut stamp));
        assert_eq!(stamp, 1);
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
