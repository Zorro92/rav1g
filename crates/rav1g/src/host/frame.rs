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

use windows::Win32::Graphics::Direct3D12::ID3D12CommandQueue;

use super::completion::start_completion_thread;
use super::create::{
    IDCT_COEF_COUNT_DENUM, rav1g_frame_buffer_release, rav1g_get_frame_buffer, rav1g_release_fb, to_mib,
};
use super::log::log;
use super::loopfilter::rav1g_loopfilter_gpu;
use super::memory::rav1g_memory_create_buffer;
use super::output::{rav1g_copy_output, rav1g_output_image};
use super::perf::{
    inter_fast_mode, perf_add, perf_add_memset, perf_begin_command_buffer, perf_detail_mode, perf_lap, perf_now,
    perf_resolve,
};
use super::postfilter::rav1g_cdef_looprestoration;
use super::prediction::{rav1g_inter_ext_borders, rav1g_prediction_gen_blocks, rav1g_prediction_run_all};
use super::stage_dump::{stage_dump_begin, stage_dump_frame_info};
use super::thread::{mutex_lock, mutex_unlock, rav1g_mt_queue_get, rav1g_mt_queue_push};
use super::transform::rav1g_idct_run;
use super::types::{
    Av1Core, DeviceUpload, EntropyThreadCount, GpuWorkItem, InterFastMapSize, IntraV2MaxRegions, av1_frame_thread_data,
    av1_perf_inter_tile,
};
use crate::gpu::d3d12::{com_ref, wrapped_ref};
use crate::gpu::{CommandAllocator, CommandList, Event, Fence, Queue};
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
    fn malloc(size: usize) -> *mut c_void;
    fn free(memory: *mut c_void);
}

/// Test player switch (-asyncclear): the per-frame clears of a slot's grids move from the decode thread
/// to the completion thread. Off unless `av1_set_async_clear(1)` was called. See [`rav1g_setup_frame_begin`]
/// for the rules.
static ASYNC_CLEAR: AtomicI32 = AtomicI32::new(0);

/// `av1_set_async_clear` (`dx/av1_perf.h`).
#[unsafe(no_mangle)]
pub extern "C" fn av1_set_async_clear(enable: c_int) {
    ASYNC_CLEAR.store(enable, Ordering::Relaxed);
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
/// `dec` is the decoder with the slot (or output context) of the frame in `curr_frame_data`; decode
/// thread.
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
            let acc = &mut (*dec).perf_cur;
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
            ybf.hw_buffer = rav1g_get_frame_buffer(dec);
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
            rav1g_frame_buffer_release(dec, ybf.hw_buffer);
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
            rav1g_frame_buffer_release(dec, ybf.hw_buffer);
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
            rav1g_mt_queue_push(&raw mut (*dec).image_pool, std::ptr::from_mut(img).cast());
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
        mutex_lock(&raw mut (*td).sec_data_mutex);
        if (*td).sec_thread_data.is_null() {
            let perf_t = super::perf::av1_perf_ticks();
            (*td).sec_thread_data = rav1g_mt_queue_get(&raw mut (*dec).frame_data_pool).cast();
            perf_add(dec, sys::AV1_PERF_CPU_WAIT_SEC_SLOT as usize, perf_t);
            if perf_t != 0 {
                (*dec).perf_cur.used_sec_slot = 1;
            }
            let sec = &*(*td).sec_thread_data;
            for i in 0..usize::try_from((*td).tile_count).unwrap_or(0) {
                (*sec.tile_data.add(i)).mi_count = 0;
            }
        }
        mutex_unlock(&raw mut (*td).sec_data_mutex);
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
        let memory = (*dec).memory;
        td.palette_buffer =
            rav1g_memory_create_buffer(memory, size as usize, DeviceUpload, c"palette_buffer".as_ptr(), td.slot_index);
        if td.palette_buffer.is_null() {
            return false;
        }
        let total: usize = (*memory).dev_bytes.iter().sum();
        log(
            false,
            &format!(
                "MEMORY palette buffer of slot {} created for frame {}, the first on the slot that allows screen \
                 content tools: {:.1} MiB; device buffers now {:.1} MiB in {} buffers",
                td.slot_index,
                td.frame_number,
                to_mib(size),
                to_mib(total as u64),
                (*memory).dev_obj_count
            ),
        );
    }
    true
}

/// A frame that was set up and never submitted (a decode error between its setup and its submit) still
/// has its slot, or its output context, in `curr_frame_data`, with the frame buffers it counted. They are
/// taken back here, before the next frame or show_existing_frame takes a slot or a context of its own.
/// Decode thread.
///
/// # Safety
/// `dec` is the decoder.
pub(crate) unsafe fn release_unsubmitted_frame(dec: *mut Av1Core) {
    // SAFETY: the caller's contract.
    unsafe {
        let Some(td) = (*dec).curr_frame_data.as_mut() else {
            return;
        };
        mutex_lock(&raw mut (*dec).fb_pool_mutex);
        if !td.back_buffer0.is_null() {
            rav1g_release_fb(dec, td.back_buffer0);
        }
        if !td.dst_frame_buffer.is_null() {
            rav1g_release_fb(dec, td.dst_frame_buffer);
        }
        for r in 0..7 {
            if !td.refs[r].is_null() {
                rav1g_release_fb(dec, td.refs[r]);
            }
            td.refs[r] = std::ptr::null_mut();
        }
        td.dst_frame_buffer = std::ptr::null_mut();
        td.back_buffer0 = std::ptr::null_mut();
        mutex_unlock(&raw mut (*dec).fb_pool_mutex);
        (*dec).curr_frame_data = std::ptr::null_mut();
        let td_ptr = std::ptr::from_mut(td).cast::<c_void>();
        if td.output_only != 0 {
            // The output copy of a show_existing_frame: nothing but the reference to the frame it was to
            // copy.
            rav1g_mt_queue_push(&raw mut (*dec).output_data_pool, td_ptr);
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
            rav1g_mt_queue_push(&raw mut (*dec).frame_data_pool, std::ptr::from_mut(sec).cast());
            td.sec_thread_data = std::ptr::null_mut();
        }
        // Its grids stay dirty and are not marked clean, so the next frame on this slot clears them itself,
        // with or without -asyncclear.
        td.clear_pending = 0;
        rav1g_mt_queue_push(&raw mut (*dec).frame_data_pool, td_ptr);
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
    // SAFETY: the caller's contract, for all of this function.
    unsafe {
        let dec = (*pbi).gpu_decoder.cast::<Av1Core>();
        let perf_setup_start = super::perf::av1_perf_ticks();
        *perf_start = perf_setup_start;
        if perf_setup_start != 0 {
            (*dec).perf_cur.t_frame_start = perf_setup_start;
        }

        release_unsubmitted_frame(dec);

        let mut perf_t = super::perf::av1_perf_ticks();
        let td_ptr = rav1g_mt_queue_get(&raw mut (*dec).frame_data_pool).cast::<av1_frame_thread_data>();
        perf_add(dec, sys::AV1_PERF_CPU_WAIT_SLOT as usize, perf_t);
        let td = &mut *td_ptr;
        td.sec_thread_data = std::ptr::null_mut();
        (*dec).curr_frame_data = td_ptr;
        td.frame_number = (*dec).frame_number;

        let cm = &mut *cm;
        let buf = &mut (*cm.cur_frame).buf;
        buf.hw_show_image = std::ptr::null_mut();

        cm.mi_grid_base = (*td.mode_info_grid).host_ptr.cast();
        cm.mi_grid_visible = cm.mi_grid_base;
        cm.mi_alloc_size = cm.mi_stride * ((cm.mi_rows + 31) & !31);
        // Test player flag -asyncclear. The two per-frame clears of the slot (this grid and the intra
        // iteration grid below) are sized from this frame, which the completion thread cannot know when it
        // returns the slot. The rule: after a frame's fence the completion thread clears exactly the bytes
        // that frame's setup cleared, and records the two sizes in the slot (clean_*). A frame skips a clear
        // only when the recorded size is exactly the size it would clear itself; then every byte of the grid
        // is what the clear on this thread would have left, inside the range and past it. Any other case
        // (first use of the slot, another frame size, a slot that went back without a completed frame, the
        // flag off) clears here as before. The marks are taken off at once: from here on the grids are being
        // written.
        let async_clear = ASYNC_CLEAR.load(Ordering::Relaxed);
        let mi_grid_bytes = size_of::<*mut *mut MB_MODE_INFO>() * cm.mi_alloc_size as usize;
        if async_clear == 0 || td.clean_mi_bytes != mi_grid_bytes {
            perf_t = super::perf::av1_perf_ticks();
            cm.mi_grid_base.cast::<u8>().write_bytes(0, mi_grid_bytes);
            perf_add_memset(dec, sys::AV1_PERF_CPU_MEMSET as usize, perf_t, mi_grid_bytes);
        }
        let clean_iter_bytes = td.clean_iter_bytes;
        td.clean_mi_bytes = 0;
        td.clean_iter_bytes = 0;
        td.async_clear = async_clear;
        td.clear_mi_bytes = mi_grid_bytes;
        td.clear_iter_bytes = 0;
        td.clear_pending = 1;

        let (y_width, y_height) = (buf.__bindgen_anon_1.widths[0], buf.__bindgen_anon_2.heights[0]);
        let grid_w = (((y_width + 63) & !63) >> 2) + 2 + 128;
        let grid_h = (((y_height + 63) & !63) >> 2) + 2 + 128;
        td.iter_grid_stride = grid_w;
        td.iter_grid_stride_uv = grid_w >> 1;
        td.bitdepth = buf.bit_depth as c_int;

        td.tile_count = cm.tile_cols * cm.tile_rows;
        (*dec).thread_count = (*pbi).max_threads.min(td.tile_count);
        // Inter breakdown (-gputimedetail): the tile threads count the blocks they push, each tile into its
        // own counters, which the prediction stage adds up when the frame is recorded.
        (*dec).perf_inter_mode = perf_detail_mode();
        if (*dec).perf_inter_mode != 0 {
            let tiles = td.tile_count as usize;
            if (*dec).perf_inter_tiles_cap < td.tile_count {
                free((*dec).perf_inter_tiles.cast());
                (*dec).perf_inter_tiles = malloc(size_of::<av1_perf_inter_tile>() * tiles).cast();
                (*dec).perf_inter_tiles_cap = if (*dec).perf_inter_tiles.is_null() { 0 } else { td.tile_count };
            }
            if (*dec).perf_inter_tiles.is_null() {
                (*dec).perf_inter_mode = 0;
            } else {
                (*dec).perf_inter_tiles.write_bytes(0, tiles);
            }
        }

        td.gen_intra_iter_y = (*td.gen_intra_inter_grid).host_ptr.cast();
        td.iter_grid_offset_uv = grid_w * grid_h;
        td.gen_intra_iter_uv = td.gen_intra_iter_y.add(td.iter_grid_offset_uv as usize);
        td.intra_stats = [0; 4];
        td.intra_v2 = intra_v2_setup_frame(dec, cm, td);
        if td.intra_v2 == 0 {
            // With -intrav2 nothing scans the iteration grid; av1_mi_push_block() writes the one entry per
            // block that gen_pred_blocks.hlsl reads.
            // The luma grid (4 bytes per entry) and the chroma grid right behind it: one range from the
            // start of the buffer. -asyncclear: skipped when the completion thread left exactly this range
            // cleared.
            let iter_grid_bytes = td.iter_grid_offset_uv as usize * 5;
            if async_clear == 0 || clean_iter_bytes != iter_grid_bytes {
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
                new_buf = rav1g_get_frame_buffer(dec);
                perf_add(dec, sys::AV1_PERF_CPU_WAIT_FB as usize, perf_t);
                td.back_buffer0 = new_buf;
            }
            describe_like(&mut *new_buf, fb, td.is_hbd);
            td.frame_buffer = new_buf;
            // back_buffer1 gets the whole description of the frame, like the back buffer above: CDEF writes
            // into it when the frame has superres and no loop restoration, and the upscale then reads its
            // visible width as the width it scales from. (It used to get the layout only; the visible size
            // was whatever an earlier frame had left there, zero at first, and the upscale of such a frame
            // scaled from that width.)
            describe_like(&mut (*dec).back_buffer1, fb, td.is_hbd);
        } else {
            td.frame_buffer = td.dst_frame_buffer;
        }

        mutex_lock(&raw mut (*dec).fb_pool_mutex);
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
        mutex_unlock(&raw mut (*dec).fb_pool_mutex);

        // From here on the slot is in a state the next setup can take back (the frame's buffers and
        // references are counted), so a failure below is reported as a decode error of this frame.
        c_int::from(!palette_buffer_for_frame(dec, cm, td))
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
        let td_ptr = (*dec).curr_frame_data;
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
        let td = &mut *(*dec).curr_frame_data;
        let cm = &*cm;
        assert!((*dec).thread_count <= EntropyThreadCount as c_int, "more tile threads than the decoder has room for");
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
        let tile = &mut *(*thread_data).tile_data;
        let dec = (*pbi).gpu_decoder.cast::<Av1Core>();
        let cur_td = (*dec).curr_frame_data;
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
        let prev_t = &*thread_data.tile_data;
        let tile_id = thread_data.tile_id as usize;
        let dec = (*pbi).gpu_decoder.cast::<Av1Core>();

        let cur_td = (*dec).curr_frame_data;
        get_sec_data(dec, cur_td);

        let td = &*(*cur_td).sec_thread_data;
        let t = &mut *td.tile_data.add(tile_id);
        t.mi_count = 0;
        t.mi_offset = prev_t.mi_offset + td.mode_info_offset - (*cur_td).mode_info_offset;
        thread_data.mi_count2 = 0;
        thread_data.mi_pool2 = (*(*dec).mode_info_pool).host_ptr.cast::<MB_MODE_INFO>().offset(t.mi_offset as isize);
        thread_data.tile_data2 = t;
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
    // SAFETY: the caller's contract, for all of this function.
    unsafe {
        let thread_data = &mut *thread_data;
        let xd: *mut MACROBLOCKD = &raw mut thread_data.xd;
        let dec = (*pbi).gpu_decoder.cast::<Av1Core>();
        let cm = &*cm;
        let tile = &*tile;
        let tile_id = tile.tile_col + tile.tile_row * cm.tile_cols;
        let td = &*(*dec).curr_frame_data;
        let t_ptr = td.tile_data.add(tile_id as usize);
        let mut perf_t = super::perf::av1_perf_ticks();
        t_ptr.write_bytes(0, 1);
        perf_add_memset(dec, sys::AV1_PERF_CPU_MEMSET_TILE as usize, perf_t, size_of::<av1_tile_data>());
        let t = &mut *t_ptr;
        thread_data.tile_data = t_ptr;
        (*xd).tile_data = t_ptr;
        if (*dec).perf_inter_mode != 0 && tile_id < (*dec).perf_inter_tiles_cap {
            t.perf_inter = (*dec).perf_inter_tiles.add(tile_id as usize).cast();
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
        thread_data.tile_id = tile_id;
        thread_data.mi_count = 0;
        thread_data.mi_count_max = mi_max_base >> (*dec).mode_info_shift;
        thread_data.mi_pool = (*(*dec).mode_info_pool).host_ptr.cast::<MB_MODE_INFO>().offset(t.mi_offset as isize);
        thread_data.mi_count2 = 0;
        thread_data.mi_pool2 = std::ptr::null_mut();
        thread_data.tile_data2 = std::ptr::null_mut();
        thread_data.ext_idct_buffer = td.ext_idct_buffer;

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
    }
}

/// The pixels of one plane of a palette block, written into the slot's palette buffer.
///
/// # Safety
/// `dst` is valid for `height` rows of `stride` samples from where the block starts; `map` has `width *
/// height` entries, each an index of the 8 colours from `colors`.
unsafe fn write_palette_plane<T: Copy>(
    mut dst: *mut T,
    stride: usize,
    mut map: *const u8,
    width: usize,
    height: usize,
    colors: &[u16],
    sample: impl Fn(u16) -> T,
) {
    for _ in 0..height {
        for j in 0..width {
            // SAFETY: the caller's contract.
            unsafe { dst.add(j).write(sample(colors[usize::from(*map.add(j))])) };
        }
        // SAFETY: the next row of the block and of its colour map.
        unsafe {
            dst = dst.add(stride);
            map = map.add(width);
        }
    }
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
        let td = &*(*dec).curr_frame_data;
        // Palette blocks exist only in frames that allow screen content tools, and the frame setup has
        // created the buffer for those.
        if td.palette_buffer.is_null() {
            return;
        }
        let fb = &*td.frame_buffer;
        let mi = &*mi;
        let params = &*params;
        let plane = plane as usize;
        let bsz = 4 >> plane;
        let palette_buf = (*td.palette_buffer).host_ptr.cast::<u8>();
        let colors = &mi.palette_mode_info.palette_colors;
        let (width, height) = (params.plane_width as usize, params.plane_height as usize);
        let plane_start = |p: usize| palette_buf.offset(fb.planes[p].offset as isize - fb.base_offset as isize);
        if fb.hbd != 0 {
            let stride = (fb.planes[plane].stride >> 1) as usize;
            let offset = ((mi.mi_row * bsz) & !3) as usize * stride + ((mi.mi_col * bsz) & !3) as usize;
            let dst = plane_start(plane).cast::<u16>().add(offset);
            write_palette_plane(dst, stride, params.color_map, width, height, &colors[8 * plane..], |c| c);
            if plane != 0 {
                let dst = plane_start(2).cast::<u16>().add(offset);
                write_palette_plane(dst, stride, params.color_map, width, height, &colors[16..], |c| c);
            }
        } else {
            let stride = fb.planes[plane].stride as usize;
            let offset = ((mi.mi_row * bsz) & !3) as usize * stride + ((mi.mi_col * bsz) & !3) as usize;
            let dst = plane_start(plane).add(offset);
            write_palette_plane(dst, stride, params.color_map, width, height, &colors[8 * plane..], |c| c as u8);
            if plane != 0 {
                let dst = plane_start(2).add(offset);
                write_palette_plane(dst, stride, params.color_map, width, height, &colors[16..], |c| c as u8);
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
    unsafe {
        let dec = (*pbi).gpu_decoder.cast::<Av1Core>();
        let cm = &raw mut (*pbi).common;
        let mut perf_t = super::perf::av1_perf_ticks();
        prepare_command_buffer(dec);
        stage_dump_frame_info(dec, cm);
        rav1g_prediction_gen_blocks(pbi, dec);
        perf_t = perf_lap(dec, sys::AV1_PERF_CPU_REC_GEN_BLOCKS as usize, perf_t);
        rav1g_idct_run(dec);
        perf_t = perf_lap(dec, sys::AV1_PERF_CPU_REC_IDCT as usize, perf_t);
        rav1g_prediction_run_all(dec, cm, std::ptr::null_mut());
        perf_t = perf_lap(dec, sys::AV1_PERF_CPU_REC_PRED as usize, perf_t);
        rav1g_loopfilter_gpu(dec, cm, &raw mut (*pbi).mb);
        perf_t = perf_lap(dec, sys::AV1_PERF_CPU_REC_LOOPFILTER as usize, perf_t);
        rav1g_cdef_looprestoration(dec, cm, (&raw mut (*pbi).lr_ctxt).cast());
        perf_t = perf_lap(dec, sys::AV1_PERF_CPU_REC_CDEF_LR as usize, perf_t);
        rav1g_inter_ext_borders(dec, cm);
        perf_t = perf_lap(dec, sys::AV1_PERF_CPU_REC_BORDERS as usize, perf_t);
        *perf_lap_start = perf_t;
        // A shown frame is output, unless it is a lower spatial layer of its temporal unit.
        (*(*dec).curr_frame_data).output_frame = c_int::from((*cm).show_frame != 0 && av1_frame_is_output(pbi) != 0);
        rav1g_output_image(dec, cm, fb)
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
            rav1g_copy_output(dec, cm, fb);
        }
        let mut perf_t = perf_lap(dec, sys::AV1_PERF_CPU_REC_COPY as usize, perf_lap_start);
        if perf_t != 0 {
            let td = &*(*dec).curr_frame_data;
            let acc = &mut (*dec).perf_cur;
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
        let item = rav1g_mt_queue_get(&raw mut (*dec).gpu_item_pool).cast::<GpuWorkItem>();
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
        rav1g_mt_queue_push(&raw mut (*dec).gpu_waiting_queue, item.cast());
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
