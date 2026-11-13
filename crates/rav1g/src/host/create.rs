// SPDX-License-Identifier: BSD-2-Clause
//
// This source code is subject to the terms of the BSD 2 Clause License and the Alliance for Open Media
// Patent License 1.0. If the BSD 2 Clause License was not distributed with this source code in the LICENSE
// file, you can obtain it at www.aomedia.org/license/software. If the Alliance for Open Media Patent
// License 1.0 was not distributed with this source code in the PATENTS file, you can obtain it at
// www.aomedia.org/license/patent.

//! Creation and release of a decoder (`av1_create_gpu_decoder`, `av1_destroy_gpu_decoder` and
//! `av1_query_memory_requirements` of `av1_core.cpp`): the plan of its buffers, the budget guard, the
//! "MEMORY" lines that say what it holds, and the pool of frame buffers.
//!
//! The decisions and the lines are those of the C++ host, number for number: an application reads them,
//! and the two decoders are compared by them. The exception is the host memory. The C++ decoder lies in
//! the host pool; this one is an allocation of its own ([`Av1Core::new`], with its allocator), and the
//! pool holds what the entropy decoder is given: its state, its buffer pool, its contexts and motion
//! vectors, the tiles of the slots. Both exist before the budget guard asks the application what it has
//! left, so the answer already has them, and the guard counts what comes after it: the device buffers.

use std::ffi::{CStr, c_int, c_void};
use std::fmt::Write as _;
use std::time::Duration;

use windows::Win32::Foundation::E_POINTER;
use windows::Win32::Graphics::Direct3D12::{ID3D12CommandQueue, ID3D12Device};
use windows::core::Interface;

use super::completion::av1_drain_gpu_decoder;
use super::compute::{upload_luts, wait_shader_create_complete};
use super::log::{clear_create_error, log, log_create_error};
use super::memory::{Allocator, Plan, log_video_memory, memory_type_name};
use super::types::{
    Av1Core, DeviceOnly, DeviceOnlyConst, DeviceUpload, FrameThreadDataCount, FrameThreadDataDefault, GpuBufferObject,
    HostRW, ImagePoolSize, InterFastMapSize, MemoryType, OutputCbSize, OutputThreadDataCount, ReadBack,
    RefFrameBufferCount, RefFrameBufferCountSharedBack, UploadUAV, av1_frame_thread_data,
};
use crate::gpu::d3d12::{com_ref, com_release, com_store, wrapped_ref};
use crate::gpu::{self, CommandAllocator, Device, Event, VideoMemory, create_hardware_device};
use crate::sys::parser::{
    AV1Decoder, BufferPool, MAX_MIB_SIZE, MB_MODE_INFO, MV_REF, RestorationUnitInfo, TPL_MV_REF, aom_codec_dec_cfg_t,
    av1_tile_data, warped_filter, wedge_mask_buf,
};
use crate::sys::{
    AV1_LOW_MEMORY_ALL, AV1_LOW_MEMORY_COEFS, AV1_LOW_MEMORY_DEFAULT, AV1_LOW_MEMORY_MODE_INFO,
    AV1_LOW_MEMORY_SHARED_BACK_BUFFER, AV1_MEMORY_GUARD_SETTLE_MS, AV1_MEMORY_GUARD_WAIT_MS,
};

/// Bytes of a transform block record of the inverse transform (`IdctBlockSize`).
const IDCT_BLOCK_SIZE: i32 = 16;
/// Coefficients of a 4x4 unit: 16 luma and 2 * 4 chroma (`IdctCoefCountDenum`).
pub(crate) const IDCT_COEF_COUNT_DENUM: i32 = 4 * 4 + 2 * 2 * 2;
/// The share of a slot in the coefficient buffer, in 24ths of the worst case: about 71 %.
const IDCT_COEF_COUNT_NUM: i32 = 17;
/// `AV1_LOW_MEMORY_COEFS`: half; two shares are exactly the worst case.
const IDCT_COEF_COUNT_NUM_LOW: i32 = 12;

/// `AV1_MEMORY_GUARD_MARGIN` of `aom/aom_decoder.h`: what a creation must leave of the application's
/// memory.
const MEMORY_GUARD_MARGIN: u64 = 48 << 20;
/// How often the budget is asked again while it grows (`PollMs`).
const GUARD_POLL_MS: u32 = 20;

/// Words of the film grain template: 96x96 luma and two chroma planes of 48x48.
const FILMGRAIN_NOISE_INTS: usize = 96 * 96 + 48 * 48 * 2;
/// The Gaussian sequence of the film grain, as ints (`dx_gaussian_sequence`).
const GAUSSIAN_SEQUENCE_BYTES: usize = size_of::<[i32; 2048]>();
/// Entries of a random register table of the film grain.
const FILMGRAIN_RANDOM_INTS: usize = 65536 + 1;

/// The size of a table of the entropy decoder, from its declaration.
const fn size_of_table<T>(_: *const T) -> usize {
    size_of::<T>()
}

pub(crate) fn to_mib(bytes: u64) -> f64 {
    bytes as f64 / (1024.0 * 1024.0)
}

/// A line of at most `capacity - 1` bytes that is put together piece by piece, as `snprintf` into a
/// buffer of `capacity` bytes does: what does not fit is cut, and the pieces behind it are left out.
struct BoundedLine {
    text: String,
    /// The length the line would have without the cut.
    wanted: usize,
    capacity: usize,
}

impl BoundedLine {
    fn new(capacity: usize) -> BoundedLine {
        BoundedLine { text: String::new(), wanted: 0, capacity }
    }

    fn full(&self) -> bool {
        self.wanted >= self.capacity
    }

    fn push(&mut self, piece: &str) {
        let room = (self.capacity - 1).saturating_sub(self.text.len());
        let mut end = piece.len().min(room);
        while !piece.is_char_boundary(end) {
            end -= 1;
        }
        self.text.push_str(&piece[..end]);
        self.wanted += piece.len();
    }
}

/// What the buffers of a decoder are sized by (`resource_config` of the C++).
struct ResourceConfig {
    bitdepth: i32,
    width: i32,
    height: i32,
    fb_count: i32,
    ref_fb_count: i32,
    gpu_pipeline_depth: i32,
    enable_superres: i32,
    /// Create the buffers of the -intrav2 path.
    intra_v2: bool,
    /// Room in the block maps for the lists of the -interfast path.
    inter_fast: bool,
    /// `AV1_LOW_MEMORY_*`.
    low_memory: i32,
}

/// Pipeline slots asked for by the decoder configuration: 0 is the default, anything else is clamped to
/// what the arrays are sized for.
fn frame_slots_from_cfg(cfg: &aom_codec_dec_cfg_t) -> i32 {
    if cfg.frame_slots == 0 {
        return FrameThreadDataDefault;
    }
    FrameThreadDataDefault.max(FrameThreadDataCount.min(cfg.frame_slots))
}

impl ResourceConfig {
    /// The same for the memory query and for the creation: both must count the same allocations.
    fn from_cfg(cfg: &aom_codec_dec_cfg_t) -> ResourceConfig {
        let gpu_pipeline_depth = frame_slots_from_cfg(cfg);
        // 0 is the default (the shared back buffer); AV1_LOW_MEMORY_NONE is how an application asks for none.
        let low_memory = if cfg.low_memory != 0 {
            cfg.low_memory & AV1_LOW_MEMORY_ALL as i32
        } else {
            AV1_LOW_MEMORY_DEFAULT as i32
        };
        // Frame buffers in the pool. A buffer is out of the pool while the entropy decoder or a frame in
        // flight holds it:
        //   - the reference map: at most 8 different buffers;
        //   - the frame being decoded: 1, taken while its header is read (av1_reallocate_frame_buffer);
        //   - its back buffer, when the frame is filtered after its reconstruction (CDEF, loop restoration,
        //     superres): 1, taken in av1_setup_frame();
        //   - a frame that failed after av1_setup_frame(): its buffer and its back buffer stay with its slot
        //     until the next av1_setup_frame(), which comes after the next frame has taken its own buffer: 2;
        //   - every other frame in flight, until its GPU work is done: its back buffer, and what the map has
        //     dropped since the frame started (its own buffer when it is not a reference, a reference it
        //     reads that a later frame replaced): 1 + 1 when frames replace one map entry each.
        // Both takes wait until a frame in flight returns a buffer, so what must never be undercut is the
        // count with nothing in flight: 8 + 2 + 1 = 11 (map, failed frame, the next frame's buffer). Below
        // that the decode thread waits for a buffer nobody returns. The 12 of the code before this is 8 for
        // the map, 1 + 1 for the frame being decoded and 1 + 1 for the one frame that is in flight next to it
        // with two slots: no wait in the common case. It is kept, and so is the one buffer more for each slot
        // above two.
        // With the shared back buffer no frame takes a back buffer from the pool: the count with nothing in
        // flight is 8 + 1 + 1 = 10 (map, failed frame, next frame), and the common case with two slots is
        // also 8 + 1 + 1 = 10 (map, frame being decoded, one dropped reference of the frame in flight).
        let base_fb_count = if low_memory & AV1_LOW_MEMORY_SHARED_BACK_BUFFER as i32 != 0 {
            RefFrameBufferCountSharedBack
        } else {
            RefFrameBufferCount
        };
        let ref_fb_count = base_fb_count + gpu_pipeline_depth - FrameThreadDataDefault;
        ResourceConfig {
            bitdepth: cfg.bitdepth as i32,
            width: cfg.width as i32,
            height: cfg.height as i32,
            fb_count: ref_fb_count,
            ref_fb_count,
            gpu_pipeline_depth,
            enable_superres: 1,
            intra_v2: cfg.intra_v2 != 0,
            inter_fast: cfg.inter_fast != 0,
            low_memory,
        }
    }
}

/// Who serves the requests of [`allocate_buffers`]: the allocator of the decoder with the decoder's
/// device, or the plan that only counts what the requests would take (`av1_memory_manager_base` of the
/// C++).
enum Requests {
    Allocator(*mut Allocator, ID3D12Device),
    Plan(Plan),
}

impl Requests {
    fn plan() -> Requests {
        Requests::Plan(Plan::default())
    }

    /// The host and device bytes a plan has counted.
    fn planned(&self) -> (usize, usize) {
        match self {
            Requests::Plan(plan) => (plan.host, plan.device),
            Requests::Allocator(..) => (0, 0),
        }
    }

    /// `create_buffer`: null from the plan, and from the allocator when the allocation failed.
    fn buffer(&mut self, size: usize, kind: MemoryType, name: &'static CStr, slot: i32) -> *mut GpuBufferObject {
        match self {
            // SAFETY: the allocator of the decoder that is being created, and its device.
            Requests::Allocator(memory, device) => unsafe {
                (**memory).create_buffer(Some(device), size, kind, name, slot)
            },
            Requests::Plan(plan) => {
                plan.create_buffer(size);
                std::ptr::null_mut()
            }
        }
    }

    /// `host_allocate`: null from the plan, and from the allocator when the pool is too small.
    fn host(&mut self, size: usize) -> *mut c_void {
        match self {
            // SAFETY: the allocator of the decoder that is being created.
            Requests::Allocator(memory, _) => unsafe { (**memory).host_allocate(size, 0) },
            Requests::Plan(plan) => {
                plan.host_allocate(size, 0);
                std::ptr::null_mut()
            }
        }
    }
}

/// `CHECK_RESULT` of the C++: the request is always made; its result is kept, and a null result ends the
/// creation, only when the requests are served by the allocator.
macro_rules! request {
    ($assign:expr, $dst:expr, $value:expr) => {{
        let value = $value;
        if $assign {
            if value.is_null() {
                return false;
            }
            $dst = value.cast();
        }
    }};
}

/// `av1_allocate_buffers`: every buffer and every piece of host memory of a decoder, in one order for the
/// plan and for the creation. The sizes the decoder remembers are written to `dec` in both. Returns
/// `false` when a request of the allocator failed (it has logged which).
///
/// # Safety
/// `dec` is valid for writing. With the allocator: `dec` is the decoder that is being created, zeroed.
unsafe fn allocate_buffers(dec: *mut Av1Core, mem: &mut Requests, cfg: &ResourceConfig) -> bool {
    let assign = matches!(mem, Requests::Allocator(..));
    // SAFETY: the caller's contract.
    let dec = unsafe { &mut *dec };
    let target_width = (cfg.width + 127) & !127;
    let target_height = (cfg.height + 127) & !127;
    let block_count_4x4 = target_width * target_height * 3 / (2 * 4 * 4);
    let mi_cols = target_width >> 2;
    let mi_rows = target_height >> 2;

    let max_tile_cols = target_width >> 6;
    let max_tile_rows = target_height >> 6;
    dec.block_count4x4 = block_count_4x4;
    dec.max_width = target_width;
    dec.max_height = target_height;
    dec.max_bitdepth = cfg.bitdepth;

    request!(assign, dec.pbi_alloc, mem.host(size_of::<AV1Decoder>()));
    request!(assign, dec.buf_pool_alloc, mem.host(size_of::<BufferPool>()));

    for p in 0..5 {
        request!(assign, dec.above_context_alloc[p], mem.host(size_of::<*mut c_void>() * max_tile_rows as usize));
    }
    for row in 0..max_tile_rows as usize {
        for p in 0..5 {
            let context = mem.host(mi_cols as usize);
            if assign {
                if context.is_null() {
                    return false;
                }
                // SAFETY: the array of `max_tile_rows` pointers allocated above.
                unsafe { dec.above_context_alloc[p].add(row).write(context) };
            }
        }
    }

    let y_max_rst_units = max_tile_cols * max_tile_rows;
    let uv_max_rst_units = (max_tile_cols >> 1) * (max_tile_rows >> 1);
    let unit = size_of::<RestorationUnitInfo>();
    request!(assign, dec.restoration_info_alloc[0], mem.host(unit * y_max_rst_units as usize));
    request!(assign, dec.restoration_info_alloc[1], mem.host(unit * uv_max_rst_units as usize));
    request!(assign, dec.restoration_info_alloc[2], mem.host(unit * uv_max_rst_units as usize));
    let tmvs_size = ((mi_rows + MAX_MIB_SIZE as i32) >> 1) * (mi_cols >> 1);
    request!(assign, dec.tplmvs_alloc, mem.host(size_of::<TPL_MV_REF>() * tmvs_size as usize));

    let residuals_size = target_width * target_height
        + 2 * ((((target_width >> 1) + 127) & !127) * (((target_height >> 1) + 127) & !127));
    request!(
        assign,
        dec.idct_residuals,
        mem.buffer(size_of::<i16>() * residuals_size as usize, DeviceOnly, c"idct_residuals", -1)
    );
    request!(
        assign,
        dec.idct_blocks,
        mem.buffer((block_count_4x4 * IDCT_BLOCK_SIZE) as usize, DeviceOnly, c"idct_blocks", -1)
    );
    let (wedge_mask_bytes, warped_filter_bytes) =
        (size_of_table(&raw const wedge_mask_buf), size_of_table(&raw const warped_filter));
    request!(assign, dec.inter_mask_lut, mem.buffer(wedge_mask_bytes * 2, DeviceOnlyConst, c"inter_mask_lut", -1));
    request!(
        assign,
        dec.inter_warp_filter,
        mem.buffer(warped_filter_bytes * 2, DeviceOnlyConst, c"inter_warp_filter", -1)
    );

    request!(
        assign,
        dec.filmgrain_noise,
        mem.buffer(size_of::<i32>() * FILMGRAIN_NOISE_INTS, DeviceOnly, c"filmgrain_noise", -1)
    );
    request!(assign, dec.filmgrain_gaus, mem.buffer(GAUSSIAN_SEQUENCE_BYTES, DeviceOnlyConst, c"filmgrain_gaus", -1));
    request!(
        assign,
        dec.filmgrain_random_luma,
        mem.buffer(size_of::<i32>() * FILMGRAIN_RANDOM_INTS, DeviceOnlyConst, c"filmgrain_random_luma", -1)
    );
    request!(
        assign,
        dec.filmgrain_random_chroma,
        mem.buffer(size_of::<i32>() * FILMGRAIN_RANDOM_INTS, DeviceOnlyConst, c"filmgrain_random_chroma", -1)
    );

    // Frame buffer size.
    let border = 16;
    let y_stride = target_width + 2 * border;
    let y_size = y_stride * target_height;
    let uv_stride = (target_width >> 1) + border * 2;
    let uv_size = uv_stride * (target_height >> 1);
    let bpp = if cfg.bitdepth > 8 { 2 } else { 1 };
    let fb_size = ((y_size + 2 * uv_size) * bpp + 255) & !255;

    // The pool buffer starts with the back buffers that are not handed out: back_buffer1, and behind it the
    // back buffer the frames share when they do not take one from the pool.
    dec.shared_back_buffer = i32::from(cfg.low_memory & AV1_LOW_MEMORY_SHARED_BACK_BUFFER as i32 != 0);
    dec.low_memory = cfg.low_memory;
    dec.fb_size = fb_size;
    dec.fb_offset = if dec.shared_back_buffer != 0 { 2 * fb_size } else { fb_size };
    dec.enable_superres = cfg.enable_superres;
    request!(
        assign,
        dec.frame_buffer_pool,
        mem.buffer(
            fb_size as usize * cfg.fb_count as usize + dec.fb_offset as usize,
            DeviceOnly,
            c"frame_buffer_pool",
            -1
        )
    );

    let grid_w = mi_cols + 2 + 128;
    let grid_h = mi_rows + 2 + 128;
    let cdef_blocks = target_width * target_height / 64 / 64;

    let block_count_8x8 = block_count_4x4 >> 2;
    let mi_size = mi_cols * mi_rows;
    let lf_blk_count = (target_width / 64) * (target_height / 4)      // vertical edges, luma
        + (target_width / 128) * (target_height / 8) * 2              // vertical edges, chroma
        + (target_height / 64) * (target_width / 4)                   // horizontal edges, luma
        + (target_height / 128) * (target_width / 8) * 2; // horizontal edges, chroma

    let max_tiles = max_tile_cols * max_tile_rows;

    dec.pred_map_size = (256 + 32) * max_tiles + (mi_size >> 2) * 10;
    // -interfast: the lists by motion vector class of every tile, behind the block maps as they always were.
    dec.inter_fast_tiles = if cfg.inter_fast { max_tiles } else { 0 };
    request!(
        assign,
        dec.prediction_blocks,
        mem.buffer((block_count_4x4 * 16 * 2) as usize, DeviceOnly, c"prediction_blocks", -1)
    );
    request!(
        assign,
        dec.prediction_blocks_warp,
        mem.buffer((block_count_8x8 * 48) as usize, DeviceOnly, c"prediction_blocks_warp", -1)
    );
    request!(
        assign,
        dec.loopfilter_blocks,
        mem.buffer((lf_blk_count * 32) as usize, DeviceOnly, c"loopfilter_blocks", -1)
    );
    // One share of the mode info pool and of the coefficient buffer per slot (mode_info_offset and
    // coef_buffer_offset below). With the default two slots these are the sizes the code always had.
    //
    // A frame has at most one mode info entry per 4x4 unit and IDCT_COEF_COUNT_DENUM (16 luma + 2 * 4
    // chroma) coefficients per 4x4 unit. A share holds half of the entries and 17/24 of the coefficients; a
    // frame that needs more takes a second slot and continues in that slot's share (av1_setup_sec_data,
    // av1_setup_ext_coef_buffer), so two shares cover the worst case. The low memory shares are a quarter
    // and 12/24: two of them no longer cover every frame, and a frame that runs out is a decode error
    // (decode_mbmi_block, av1_read_coeffs_txb).
    dec.mode_info_shift = if cfg.low_memory & AV1_LOW_MEMORY_MODE_INFO as i32 != 0 { 2 } else { 1 };
    dec.coef_count_num =
        if cfg.low_memory & AV1_LOW_MEMORY_COEFS as i32 != 0 { IDCT_COEF_COUNT_NUM_LOW } else { IDCT_COEF_COUNT_NUM };
    let mi_share = mi_size >> dec.mode_info_shift;
    let slot_count = cfg.gpu_pipeline_depth as usize;
    request!(
        assign,
        dec.mode_info_pool,
        mem.buffer(size_of::<MB_MODE_INFO>() * mi_share as usize * slot_count, HostRW, c"mode_info_pool", -1)
    );
    let coef_buffer_size = target_width * target_height * 3 * dec.coef_count_num / (IDCT_COEF_COUNT_DENUM * 2);
    request!(
        assign,
        dec.idct_coefs,
        mem.buffer(size_of::<i32>() * coef_buffer_size as usize * slot_count, UploadUAV, c"idct_coefs", -1)
    );
    let grain_offsets = size_of::<i32>() * ((target_width / 32 + 4) * (target_height / 32 + 1)) as usize;
    for i in 0..cfg.gpu_pipeline_depth {
        let td = &mut dec.frame_thread_data[i as usize];
        td.slot_index = i;
        td.mode_info_max = mi_share;
        td.mode_info_offset = i * mi_share;
        td.coef_buffer_offset = i * coef_buffer_size;
        td.coef_buffer_size = coef_buffer_size * size_of::<i32>() as i32;
        request!(assign, td.command_buffer.cb_alloc, mem.buffer(1024 * 1024, DeviceUpload, c"cb_alloc", i));
        request!(assign, td.tile_data, mem.host(size_of::<av1_tile_data>() * max_tiles as usize));
        request!(
            assign,
            td.gen_mi_block_indexes,
            mem.buffer(size_of::<i32>() * block_count_4x4 as usize * 2, HostRW, c"gen_mi_block_indexes", i)
        );
        request!(
            assign,
            td.gen_intra_inter_grid,
            mem.buffer((grid_w * grid_h * 6) as usize, HostRW, c"gen_intra_inter_grid", i)
        );
        request!(
            assign,
            td.gen_block_map,
            mem.buffer(
                (dec.pred_map_size + dec.inter_fast_tiles * InterFastMapSize) as usize * size_of::<i32>(),
                HostRW,
                c"gen_block_map",
                i
            )
        );
        // Test player flag -intrav2 (intra_v2_sched.h): 8 bytes per 4x4 cell of luma and chroma, then the
        // region list, which cannot have more entries than there are cells; two words per 64x64 region on
        // the host. Only for a decoder that was created for that path (aom_codec_dec_cfg_t::intra_v2):
        // nothing else reads or writes them, and the frame setup keeps the original shaders when they are
        // not there.
        td.intra_v2_cells = 0;
        if cfg.intra_v2 {
            td.intra_v2_cells = mi_size + (mi_size >> 2);
            request!(
                assign,
                td.intra_v2_buffer,
                mem.buffer(td.intra_v2_cells as usize * 16, HostRW, c"intra_v2_buffer", i)
            );
            request!(assign, td.intra_v2_regions, mem.host(size_of::<i32>() * 2 * (mi_size >> 8) as usize));
        }
        td.intra_v2_sched.stamp = 0;
        td.intra_v2_cols = 0;
        td.intra_v2_rows = 0;
        td.async_clear = 0;
        td.clear_pending = 0;
        td.clear_mi_bytes = 0;
        td.clear_iter_bytes = 0;
        td.clean_mi_bytes = 0;
        td.clean_iter_bytes = 0;
        request!(
            assign,
            td.mode_info_grid,
            mem.buffer(size_of::<*mut MB_MODE_INFO>() * mi_size as usize, HostRW, c"mode_info_grid", i)
        );
        request!(
            assign,
            td.idct_blocks_unordered,
            mem.buffer((block_count_4x4 * IDCT_BLOCK_SIZE) as usize, DeviceUpload, c"idct_blocks_unordered", i)
        );

        request!(
            assign,
            td.cdef_indexes,
            mem.buffer(size_of::<i32>() * cdef_blocks as usize, DeviceUpload, c"cdef_indexes", i)
        );
        request!(
            assign,
            td.cdef_skips,
            mem.buffer(size_of::<i32>() * cdef_blocks as usize * 16 * 16, DeviceUpload, c"cdef_skips", i)
        );
        request!(
            assign,
            td.loop_rest_types,
            mem.buffer((16 * (block_count_4x4 >> 8)) as usize, DeviceUpload, c"loop_rest_types", i)
        );
        request!(
            assign,
            td.loop_rest_wiener,
            mem.buffer((64 * (block_count_4x4 >> 8)) as usize, DeviceUpload, c"loop_rest_wiener", i)
        );
        request!(
            assign,
            td.filmgrain_rand_offset,
            mem.buffer(grain_offsets, DeviceUpload, c"filmgrain_rand_offset", i)
        );

        // No palette buffer here: the frame setup creates the slot's on the first frame that can have
        // palette blocks.
    }
    // Output contexts (av1_decode_sef): the constants of one output copy and the film grain offsets of a
    // frame.
    for i in 0..OutputThreadDataCount as usize {
        let td = &mut dec.output_thread_data[i];
        td.output_only = 1;
        td.slot_index = -1;
        request!(
            assign,
            td.command_buffer.cb_alloc,
            mem.buffer(OutputCbSize as usize, DeviceUpload, c"output_cb_alloc", -1)
        );
        request!(
            assign,
            td.filmgrain_rand_offset,
            mem.buffer(grain_offsets, DeviceUpload, c"output_filmgrain_rand_offset", -1)
        );
    }
    dec.palette_size = fb_size as usize;

    let mvs_size = ((target_width >> 3) * (target_height >> 3)) as usize * size_of::<MV_REF>();
    let seg_size = ((target_width >> 2) * (target_height >> 2)) as usize;
    for i in 0..cfg.ref_fb_count as usize {
        let fb = &mut dec.fb_pool_src[i];
        request!(assign, fb.mvs_alloc, mem.host(mvs_size));
        request!(assign, fb.seg_alloc, mem.host(seg_size));
    }
    true
}

/// The bytes of the palette buffers of a decoder: one per slot, created later, when a stream needs it.
fn deferred_device_bytes(dec: &Av1Core, rcfg: &ResourceConfig) -> usize {
    rcfg.gpu_pipeline_depth as usize * ((dec.palette_size + 255) & !255)
}

/// The device bytes [`allocate_buffers`] creates for `rcfg`, and the bytes of the buffers that are created
/// later. `dec` is only written where the creation writes the same values.
///
/// # Safety
/// `dec` is valid for writing.
unsafe fn planned_device_bytes(dec: *mut Av1Core, rcfg: &ResourceConfig) -> (usize, usize) {
    let mut plan = Requests::plan();
    // SAFETY: the caller's contract; a plan assigns nothing.
    unsafe {
        allocate_buffers(dec, &mut plan, rcfg);
        (plan.planned().1, deferred_device_bytes(&*dec, rcfg))
    }
}

/// The private data of the codec interface (`aom_codec_alg_priv_t` of `av1_dx_iface.c`, about 20 KB),
/// which lies at the start of the host pool, with room for the pool to start at a multiple of 256.
const INTERFACE_BYTES: usize = 32 * 1024;

/// What a decoder takes of the heap of the process besides its host pool: the decoder itself and its
/// allocator ([`Av1Core::own_bytes`]). For the memory query of the Rust interface, which reports all host
/// memory of a decoder.
pub(crate) const fn decoder_own_bytes() -> usize {
    Av1Core::own_bytes()
}

/// `av1_query_memory_requirements` (`dx/av1_core.h`): what a decoder of this configuration takes of the
/// host pool and of device memory. The host pool is what the entropy decoder is given; the decoder itself
/// is not in it ([`decoder_own_bytes`]).
///
/// # Safety
/// `cfg` is the configuration of a decoder.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn av1_query_memory_requirements(cfg: *mut aom_codec_dec_cfg_t) -> c_int {
    // SAFETY: the caller's contract.
    let cfg = unsafe { &mut *cfg };
    let mut plan = Requests::plan();
    plan.host(INTERFACE_BYTES);
    let rcfg = ResourceConfig::from_cfg(cfg);
    // A decoder for the sizes the plan writes; nothing else of it is used.
    let Some(mut dec) = Av1Core::new() else {
        return -1;
    };
    // SAFETY: a plan assigns nothing.
    unsafe { allocate_buffers(&raw mut *dec, &mut plan, &rcfg) };
    let (host, device) = plan.planned();
    cfg.host_size = host;
    cfg.device_size = device;
    cfg.device_size_deferred = deferred_device_bytes(&dec, &rcfg);
    0
}

/// "MEMORY" lines about what the decoder holds: the buffers by memory type, the largest ones, the host
/// pool, and what one pipeline slot takes. After a creation, and at the point where a creation failed.
///
/// # Safety
/// `dec` is a decoder with its allocator.
unsafe fn log_memory_summary(is_error: bool, label: &str, dec: &Av1Core, rcfg: &ResourceConfig) {
    // SAFETY: the allocator of the decoder (the caller's contract).
    let mem = unsafe { &*dec.memory };
    let buffers = mem.buffers();
    let total = mem.device_bytes();
    let shared = dec.shared_back_buffer != 0;
    log(
        is_error,
        &format!(
            "MEMORY {label}: {}x{} {}-bit, slots {}, frame buffers {} + {} back buffer{} of {} bytes ({:.1} MiB) \
             each; {} device buffers, {total} bytes ({:.1} MiB); host pool {} of {} bytes ({:.1} MiB) in use",
            rcfg.width as u32,
            rcfg.height as u32,
            rcfg.bitdepth as u32,
            rcfg.gpu_pipeline_depth,
            rcfg.ref_fb_count,
            if shared { 2 } else { 1 },
            if shared { "s" } else { "" },
            dec.fb_size,
            to_mib(dec.fb_size as u64),
            mem.dev_obj_count,
            to_mib(total as u64),
            mem.host_offset,
            mem.host_max_size,
            to_mib(mem.host_offset as u64)
        ),
    );
    // What was left out or made smaller, and what is still to come: a slot's palette buffer is created on
    // the first frame of the slot that allows screen content tools.
    let palette_buffers = dec.frame_thread_data[..rcfg.gpu_pipeline_depth as usize]
        .iter()
        .filter(|td| !td.palette_buffer.is_null())
        .count();
    log(
        is_error,
        &format!(
            "MEMORY {label} options: intra_v2 buffers {}; low memory {} (shared back buffer {}, mode info share \
             1/{} of the worst case, coefficient share {}/{}); palette buffers {palette_buffers} of {} created, \
             {:.1} MiB each, a slot's on its first frame with screen content tools",
            if rcfg.intra_v2 { "yes" } else { "no (the decoder was not created for -intrav2)" },
            rcfg.low_memory,
            if shared { "yes" } else { "no" },
            1 << dec.mode_info_shift,
            dec.coef_count_num,
            IDCT_COEF_COUNT_DENUM,
            rcfg.gpu_pipeline_depth,
            to_mib(dec.palette_size as u64)
        ),
    );

    // Host memory: the pool, and what the decoder allocates next to it. Not a line of the C++ decoder,
    // which lies in its pool.
    log(
        is_error,
        &format!(
            "MEMORY {label} host memory: host pool {} bytes ({:.1} MiB) for the entropy decoder, {} of them in \
             use; the decoder itself {} bytes ({:.1} MiB) on the heap, not in the pool; both allocated before the \
             budget is asked",
            mem.host_max_size,
            to_mib(mem.host_max_size as u64),
            mem.host_offset,
            Av1Core::own_bytes(),
            to_mib(Av1Core::own_bytes() as u64)
        ),
    );

    let type_name = memory_type_name;
    let mut line = BoundedLine::new(640);
    for kind in 0..gpu::MemoryKind::COUNT {
        if line.full() {
            break;
        }
        line.push(&format!(
            "{}{} {} buffers {:.1} MiB",
            if kind != 0 { "; " } else { "" },
            type_name(kind as MemoryType),
            mem.dev_count[kind],
            to_mib(mem.dev_bytes[kind] as u64)
        ));
    }
    // The decoder creates buffers only. The output images are textures of the application.
    log(is_error, &format!("MEMORY {label} by type: {}; textures 0", line.text));

    // One slot: its own buffers and its share of the two buffers that are divided between the slots.
    let mut slot_own = 0usize;
    let mut slot_buffers = 0;
    let mut pool_share = 0usize;
    let mut coef_share = 0usize;
    for obj in buffers {
        if obj.slot == 0 {
            slot_own += obj.size;
            slot_buffers += 1;
        }
        if std::ptr::eq(obj, dec.mode_info_pool) {
            pool_share = obj.size / rcfg.gpu_pipeline_depth as usize;
        }
        if std::ptr::eq(obj, dec.idct_coefs) {
            coef_share = obj.size / rcfg.gpu_pipeline_depth as usize;
        }
    }
    let slot_share = slot_own + pool_share + coef_share;
    log(
        is_error,
        &format!(
            "MEMORY {label} per slot: {slot_share} bytes ({:.1} MiB) = {slot_buffers} own buffers {:.1} MiB + \
             mode_info_pool share {:.1} MiB + idct_coefs share {:.1} MiB; a slot above {} also adds one frame \
             buffer, {:.1} MiB",
            to_mib(slot_share as u64),
            to_mib(slot_own as u64),
            to_mib(pool_share as u64),
            to_mib(coef_share as u64),
            FrameThreadDataDefault,
            to_mib(dec.fb_size as u64)
        ),
    );

    // The largest buffers, one entry per name: the buffers of the slots are the same in every slot.
    const LARGEST: usize = 10;
    let mut top: Vec<&GpuBufferObject> = Vec::with_capacity(LARGEST + 1);
    for obj in buffers {
        if obj.slot > 0 {
            continue;
        }
        let mut pos = top.len();
        while pos > 0 && top[pos - 1].size < obj.size {
            pos -= 1;
        }
        if pos >= LARGEST {
            continue;
        }
        top.insert(pos, obj);
        top.truncate(LARGEST);
    }
    let mut line = BoundedLine::new(640);
    for (i, obj) in top.iter().enumerate() {
        if line.full() {
            break;
        }
        // SAFETY: the name of a buffer is a static C string (create_buffer).
        let name = unsafe { CStr::from_ptr(obj.name) }.to_string_lossy();
        line.push(&format!(
            "{}{name} {:.1} MiB {}{}",
            if i != 0 { "; " } else { "" },
            to_mib(obj.size as u64),
            type_name(obj.memtype),
            if obj.slot == 0 { " (per slot)" } else { "" }
        ));
    }
    log(is_error, &format!("MEMORY {label} largest: {}", line.text));
}

/// The device of the decoder with a reference of its own, for the lines behind the decoder's release.
///
/// # Safety
/// `dec` is a decoder; its device field is null or holds a device.
unsafe fn device_of(dec: *const Av1Core) -> Option<ID3D12Device> {
    // SAFETY: the caller's contract.
    unsafe { com_ref::<_, ID3D12Device>(&(*dec).compute.device).cloned() }
}

/// A step of the decoder creation failed. Says which one, what was allocated up to there and what the
/// video memory budget is, then releases everything the creation has built, so that nothing of it is left
/// when the application tries again: the caller never gets a decoder to destroy.
///
/// # Safety
/// `dec` is the decoder that is being created.
unsafe fn create_failed(
    dec: *mut Av1Core,
    rcfg: &ResourceConfig,
    before: Option<&VideoMemory>,
    step: &str,
    hr: i32,
) -> c_int {
    // An allocation of the decoder's allocator has logged itself already and is the reason kept for the
    // application; this line is for every other step.
    if hr < 0 {
        log_create_error(&format!("MEMORY FAILED decoder creation at: {step} (hr 0x{:08X})", hr as u32));
    } else {
        log_create_error(&format!("MEMORY FAILED decoder creation at: {step}"));
    }
    // SAFETY: the caller's contract; the decoder has its allocator from the start of the creation.
    unsafe {
        log_memory_summary(true, "FAILED decoder, allocated up to the failure", &*dec, rcfg);
        // Keeps the device for the query after the release.
        let device = device_of(dec);
        if let Some(device) = &device {
            let now = Device::video_memory(Some(device));
            if let Some(before) = before {
                log_video_memory(true, "FAILED, before the creation", before);
            }
            log_video_memory(true, "FAILED, at the failure", &now);
        }
        destroy(dec, false);
        if let Some(device) = &device {
            let released = Device::video_memory(Some(device));
            log_video_memory(true, "FAILED, after releasing the failed decoder", &released);
        }
    }
    -1
}

/// `create_device` of the C++: the decoder's device (the first hardware adapter when the application gave
/// none) and its queue (a direct queue of its own when the application gave none).
///
/// # Safety
/// `dec` is the decoder that is being created; its device and queue fields are null or hold a reference.
unsafe fn create_device(dec: &mut Av1Core) -> Result<(), gpu::Error> {
    // SAFETY: the caller's contract; an empty field gets the reference of what was created.
    unsafe {
        if com_ref::<_, ID3D12Device>(&dec.compute.device).is_none() {
            com_store(&raw mut dec.compute.device, create_hardware_device()?.into_raw());
        }
        let device = com_ref::<_, ID3D12Device>(&dec.compute.device).expect("the device was stored");
        if com_ref::<_, ID3D12CommandQueue>(&dec.compute.queue).is_none() {
            com_store(&raw mut dec.compute.queue, Device::new(device).create_queue()?.into_raw());
        }
        let queue = com_ref::<_, ID3D12CommandQueue>(&dec.compute.queue).expect("the queue was stored");
        com_store(&raw mut dec.compute.queue_direct, queue.clone().into_raw());
    }
    Ok(())
}

/// The command allocator, the fence and the event of a slot or of an output context.
///
/// # Safety
/// `td` is a slot of the decoder that is being created; its fields are empty.
unsafe fn create_command_buffer(
    device: Device<'_>,
    td: &mut av1_frame_thread_data,
    what: [&'static str; 3],
) -> Result<(), (&'static str, i32)> {
    let cb = &mut td.command_buffer;
    let allocator = device.create_command_allocator().map_err(|error| (what[0], error.code))?;
    // SAFETY: an empty field (the caller's contract).
    unsafe { com_store(&raw mut cb.allocator, allocator.into_raw()) };
    cb.fence_value = 0;
    let fence = device.create_fence().map_err(|error| (what[1], error.code))?;
    // SAFETY: as above.
    unsafe { com_store(&raw mut cb.fence, fence.into_raw()) };
    cb.event = Event::new().map_err(|error| (what[2], error.code))?.into_raw();
    td.frame_number = 0;
    Ok(())
}

/// `av1_create_gpu_decoder` (`dx/av1_core.h`): a decoder on its device, with every buffer it needs and
/// with the host pool of `cfg` for the entropy decoder. Returns 0 and the decoder, or -1 after a "MEMORY
/// FAILED" or "MEMORY REFUSED" line; then nothing is left of it.
///
/// # Safety
/// `gpu_dec` is valid for writing; `cfg` is the configuration the host pool was sized for.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn av1_create_gpu_decoder(gpu_dec: *mut *mut Av1Core, cfg: *mut aom_codec_dec_cfg_t) -> c_int {
    // SAFETY: the caller's contract, for all of this function.
    unsafe {
        *gpu_dec = std::ptr::null_mut();
        clear_create_error();
        let cfg = &*cfg;

        // The decoder and its allocator: allocations of their own, empty, with the queues and the locks
        // made. The release is valid from here on, whichever step below fails.
        let (Some(dec), Some(mem)) = (Av1Core::new(), Allocator::new_boxed(cfg.host_memory.cast(), cfg.host_size))
        else {
            log_create_error(&format!(
                "MEMORY FAILED decoder: the process has no memory for the decoder's {} bytes",
                Av1Core::own_bytes()
            ));
            return -1;
        };
        let dec = Box::into_raw(dec);
        let mem = Box::into_raw(mem);
        (*dec).memory = mem;

        let rcfg = ResourceConfig::from_cfg(cfg);
        (*dec).parser_threads = c_int::try_from(cfg.threads).unwrap_or(1);
        (*dec).frame_slots = rcfg.gpu_pipeline_depth;
        let frame_slots = rcfg.gpu_pipeline_depth as usize;

        // The decoder takes a reference of the device and of the queue the application gave.
        if let Some(device) = ID3D12Device::from_raw_borrowed(&cfg.dx12device) {
            com_store(&raw mut (*dec).compute.device, device.clone().into_raw());
        }
        if let Some(queue) = ID3D12CommandQueue::from_raw_borrowed(&cfg.dx12command_queue) {
            com_store(&raw mut (*dec).compute.queue, queue.clone().into_raw());
        }
        if let Err(error) = create_device(&mut *dec) {
            return create_failed(dec, &rcfg, None, "D3D12 device or command queue", error.code);
        }
        let device = device_of(dec).expect("the decoder has a device");

        (*dec).cb_get_output_image = cfg.out_buffers_cb.get_out_buffer_cb;
        (*dec).cb_release_image = cfg.out_buffers_cb.release_out_buffer_cb;
        (*dec).cb_notify_frame_ready = cfg.out_buffers_cb.notify_frame_ready_cb;
        (*dec).image_alloc_priv = cfg.out_buffers_cb.out_buffers_priv;

        // What the adapter reports before the decoder takes anything.
        let vm_before = Device::video_memory(Some(&device));

        (*dec).shader_lib = cfg.dxPsos.cast();
        if (*dec).shader_lib.is_null() {
            return create_failed(dec, &rcfg, Some(&vm_before), "no pipeline cache (dxPsos) given", E_POINTER.0);
        }
        let shader_errors = wait_shader_create_complete((*dec).shader_lib);
        if shader_errors != 0 {
            return create_failed(dec, &rcfg, Some(&vm_before), "compute pipeline states (shaders)", shader_errors);
        }

        // Budget guard: an application over its memory limit loses the D3D12 device for the rest of the
        // process, so with a budget callback the creation is refused before it allocates anything it cannot
        // afford.
        (*dec).memory_available_cb = cfg.memory_available_cb;
        (*dec).memory_available_priv = cfg.memory_available_priv;
        {
            let (planned, deferred) = planned_device_bytes(dec, &rcfg);
            let callback = (*dec).memory_available_cb;
            let private = (*dec).memory_available_priv;
            let mut available = callback.map_or(-1, |callback| callback(private));
            let needed = planned as u64 + MEMORY_GUARD_MARGIN;
            if let Some(callback) = callback.filter(|_| available >= 0 && needed > available as u64) {
                // Not refused at once. The system gives the memory of released D3D12 buffers back some time
                // after the release (a decoder that was destroyed just before this creation), so the answer
                // is asked again for as long as it keeps growing. Bounded: AV1_MEMORY_GUARD_WAIT_MS, and
                // AV1_MEMORY_GUARD_SETTLE_MS without a gain of 1 MiB ends it.
                let first = available;
                let mut best = available;
                let mut waited_ms = 0u32;
                let mut settle_ms = 0u32;
                while needed > available as u64
                    && waited_ms < AV1_MEMORY_GUARD_WAIT_MS
                    && settle_ms < AV1_MEMORY_GUARD_SETTLE_MS
                {
                    std::thread::sleep(Duration::from_millis(u64::from(GUARD_POLL_MS)));
                    waited_ms += GUARD_POLL_MS;
                    available = callback(private);
                    if available < 0 {
                        break;
                    }
                    if available >= best + (1 << 20) {
                        best = available;
                        settle_ms = 0;
                    } else {
                        settle_ms += GUARD_POLL_MS;
                    }
                }
                if available >= 0 {
                    log(
                        false,
                        &format!(
                            "MEMORY budget: {:.1} MiB needed and {:.1} MiB left for the decoder, waited {waited_ms} \
                             ms for memory the system may still be returning: {:.1} MiB left now ({})",
                            to_mib(needed),
                            to_mib(first as u64),
                            to_mib(available as u64),
                            if needed <= available as u64 {
                                "enough"
                            } else if waited_ms >= AV1_MEMORY_GUARD_WAIT_MS {
                                "still growing at the end of the wait"
                            } else {
                                "no longer growing"
                            }
                        ),
                    );
                }
            }
            if available >= 0 && needed > available as u64 {
                log_create_error(&format!(
                    "MEMORY REFUSED decoder {}x{} {}-bit, slots {}: its device buffers take {planned} bytes ({:.1} \
                     MiB) and the creation must leave a margin of {:.1} MiB, {:.1} MiB together; the application \
                     has {:.1} MiB left for the decoder. Nothing was allocated; the D3D12 device is intact",
                    rcfg.width as u32,
                    rcfg.height as u32,
                    rcfg.bitdepth as u32,
                    rcfg.gpu_pipeline_depth,
                    to_mib(planned as u64),
                    to_mib(MEMORY_GUARD_MARGIN),
                    to_mib(needed),
                    to_mib(available as u64)
                ));
                log_video_memory(true, "REFUSED, nothing allocated", &vm_before);
                destroy(dec, false);
                return -1;
            }
            if available >= 0 {
                log(
                    false,
                    &format!(
                        "MEMORY budget: device buffers {:.1} MiB + margin {:.1} MiB = {:.1} MiB, the application \
                         has {:.1} MiB left for the decoder: accepted; palette buffers on first use would add up \
                         to {:.1} MiB",
                        to_mib(planned as u64),
                        to_mib(MEMORY_GUARD_MARGIN),
                        to_mib(needed),
                        to_mib(available as u64),
                        to_mib(deferred as u64)
                    ),
                );
            } else {
                log(
                    false,
                    &format!(
                        "MEMORY budget: not checked ({}); device buffers {:.1} MiB, palette buffers on first use \
                         would add up to {:.1} MiB",
                        if callback.is_some() {
                            "the application's callback does not know what is left"
                        } else {
                            "no memory_available_cb in the decoder configuration"
                        },
                        to_mib(planned as u64),
                        to_mib(deferred as u64)
                    ),
                );
            }
        }

        if !allocate_buffers(dec, &mut Requests::Allocator(mem, device.clone()), &rcfg) {
            let step = "decoder buffers (the MEMORY FAILED line above names the one)";
            return create_failed(dec, &rcfg, Some(&vm_before), step, 0);
        }
        let coefs = &*(*dec).idct_coefs;
        coefs.host_ptr.cast::<u8>().write_bytes(0, coefs.size);

        (*dec).tryhdr10x3 = cfg.tryHDR10x3;

        for i in 0..frame_slots {
            let td = &mut (*dec).frame_thread_data[i];
            let what = ["command allocator of a slot", "fence of a slot", "event of a slot"];
            if let Err((step, hr)) = create_command_buffer(Device::new(&device), td, what) {
                return create_failed(dec, &rcfg, Some(&vm_before), step, hr);
            }
            (*dec).frame_data_pool.push(std::ptr::from_mut(td));
        }
        for i in 0..OutputThreadDataCount as usize {
            let td = &mut (*dec).output_thread_data[i];
            let what =
                ["command allocator of an output context", "fence of an output context", "event of an output context"];
            if let Err((step, hr)) = create_command_buffer(Device::new(&device), td, what) {
                return create_failed(dec, &rcfg, Some(&vm_before), step, hr);
            }
            (*dec).output_data_pool.push(std::ptr::from_mut(td));
        }

        // The command list of the decoder, open on the allocator of the first slot, and closed until a
        // frame is recorded.
        let allocator = wrapped_ref::<_, CommandAllocator>(&(*dec).frame_thread_data[0].command_buffer.allocator);
        let allocator = allocator.expect("the first slot has its allocator");
        let list = match Device::new(&device).create_command_list(allocator) {
            Ok(list) => list,
            Err(error) => return create_failed(dec, &rcfg, Some(&vm_before), "command list", error.code),
        };
        let closed = list.close();
        com_store(&raw mut (*dec).compute.command_list, list.into_raw());
        if let Err(error) = closed {
            return create_failed(dec, &rcfg, Some(&vm_before), "command list Close", error.code);
        }

        let hr = upload_luts(dec);
        if hr < 0 {
            return create_failed(dec, &rcfg, Some(&vm_before), "upload of the lookup tables", hr);
        }

        // Work items (GpuItemPoolSize). The decode thread waits when the pool is empty.
        for i in 0..(*dec).gpu_item_pool_src.len() {
            let item = &raw mut (*dec).gpu_item_pool_src[i];
            (*item).data = std::ptr::null_mut();
            (*item).image = std::ptr::null_mut();
            (*dec).gpu_item_pool.push(item);
        }

        for i in 0..ImagePoolSize as usize {
            let img = &raw mut (*dec).image_pool_src[i];
            (*img).size = 0;
            (*img).fb_ptr = std::ptr::null_mut();
            (*img).is_valid = 0;
            (*dec).image_pool.push(img);
        }

        let fb_size = (*dec).fb_size;
        (*dec).back_buffer1.size = fb_size as _;
        (*dec).back_buffer1.base_offset = 0;
        // Only used with shared_back_buffer; the pool buffer has the room for it only then.
        (*dec).back_buffer0.size = fb_size as _;
        (*dec).back_buffer0.base_offset = fb_size as _;
        for i in 0..rcfg.ref_fb_count {
            let offset = (*dec).fb_offset + fb_size * i;
            let fb = &raw mut (*dec).fb_pool_src[i as usize];
            (*fb).size = fb_size as _;
            (*fb).base_offset = offset as _;
            (*fb).ref_cnt = 0;
            (*dec).fb_pool.lock().add(fb);
        }

        log_memory_summary(false, "decoder created", &*dec, &rcfg);
        let vm_after = Device::video_memory(Some(&device));
        log_video_memory(false, "before the decoder's allocations", &vm_before);
        log_video_memory(false, "after the decoder's allocations", &vm_after);

        *gpu_dec = dec;
    }
    0
}

/// `av1_allocate_pbi` (`dx/av1_core.h`): the entropy decoder's state and its buffer pool, zeroed, in the
/// host memory the creation set aside for them.
///
/// # Safety
/// `dec` is a decoder; `ppbi` and `pbp` are valid for writing.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn av1_allocate_pbi(dec: *mut Av1Core, ppbi: *mut *mut AV1Decoder, pbp: *mut *mut BufferPool) {
    // SAFETY: the caller's contract; the two pieces of host memory have the sizes of the structs.
    unsafe {
        let pbi = (*dec).pbi_alloc.cast::<AV1Decoder>();
        let bp = (*dec).buf_pool_alloc.cast::<BufferPool>();
        pbi.write_bytes(0, 1);
        bp.write_bytes(0, 1);
        (*pbi).gpu_decoder = dec.cast();
        *ppbi = pbi;
        *pbp = bp;
    }
}

// ---------------------------------------------------------------------------------------------------------
// Release.
// ---------------------------------------------------------------------------------------------------------

/// One "MEMORY destroy <step>: ..." line: what the adapter reports and what the application has left (its
/// budget callback) at a step of the release of a decoder. The differences between the lines say which
/// step gave how much back, to the video memory and to the application.
fn log_destroy_step(
    device: &ID3D12Device,
    available_cb: Option<unsafe extern "C" fn(*mut c_void) -> i64>,
    available_priv: *mut c_void,
    step: &str,
) {
    let vm = Device::video_memory(Some(device));
    let dxgi = if vm.adapter_error >= 0 && vm.local.error >= 0 {
        format!("dxgi local usage {:.1} MiB", to_mib(vm.local.usage))
    } else {
        String::from("dxgi local usage not available")
    };
    // SAFETY: the application's callback with its own pointer.
    let left = available_cb.map_or(-1, |callback| unsafe { callback(available_priv) });
    let app = if left >= 0 {
        format!("the application has {:.1} MiB left for a decoder", to_mib(left as u64))
    } else {
        format!(
            "what the application has left is not known ({})",
            if available_cb.is_some() { "its callback does not know" } else { "no memory_available_cb" }
        )
    };
    log(false, &format!("MEMORY destroy {step}: {dxgi}; {app}"));
}

/// What the destructor of the C++ `Av1Core` released: the interfaces of the slots and of the output
/// contexts (the timing query heaps and their readback buffers, the copies of -dumpstages, fence and
/// allocator), then the command list, the decoder's queue (its own unless the application gave one) and
/// its reference to the device.
///
/// # Safety
/// `dec` is a decoder nothing uses any more.
unsafe fn release_interfaces(dec: *mut Av1Core) {
    // SAFETY: the caller's contract: each field is null or holds a reference of the decoder.
    unsafe {
        let slots = (*dec).frame_thread_data.iter_mut().rev();
        for td in (*dec).output_thread_data.iter_mut().rev().chain(slots) {
            for dump in td.dumps.iter_mut().rev() {
                com_release(&raw mut dump.readback);
            }
            com_release(&raw mut td.perf.readback);
            com_release(&raw mut td.perf.query_heap);
            com_release(&raw mut td.command_buffer.fence);
            com_release(&raw mut td.command_buffer.allocator);
        }
        com_release(&raw mut (*dec).compute.command_list);
        com_release(&raw mut (*dec).compute.queue_direct);
        com_release(&raw mut (*dec).compute.queue);
        com_release(&raw mut (*dec).compute.device);
    }
}

/// `destroy_gpu_decoder` of the C++. Also for a decoder whose creation stopped half way: everything not
/// built yet is zero. `log_steps`: the release of a decoder that was in use, logged step by step
/// ("MEMORY destroy"). The decoder and its allocator are freed at the end.
///
/// # Safety
/// `dec` is a decoder of [`Av1Core::new`] with its allocator, or one whose creation failed; nothing uses
/// it afterwards.
unsafe fn destroy(dec: *mut Av1Core, log_steps: bool) {
    // SAFETY: the caller's contract, for all of this function.
    unsafe {
        // The device stays with the application; it is held here for the lines after the decoder's own
        // reference is gone. The budget callback is the application's.
        let device = if log_steps { device_of(dec) } else { None };
        let available_cb = (*dec).memory_available_cb;
        let available_priv = (*dec).memory_available_priv;
        if let Some(device) = &device {
            log_destroy_step(device, available_cb, available_priv, "start");
        }

        // Joins the GPU completion thread: every frame that was submitted has completed on the GPU (the
        // thread waits for the fence of each), so nothing on the queue uses the buffers that are released
        // below.
        av1_drain_gpu_decoder(dec);
        // The parser of phase 2: its threads are joined before the buffers they write are released.
        drop((*dec).rav1d.take());

        if let Some(release_image) = (*dec).cb_release_image {
            for img in &(*dec).image_pool_src {
                if img.is_valid != 0 {
                    release_image((*dec).image_alloc_priv, img.alloc_priv);
                }
            }
        }

        let frame_slots = usize::try_from((*dec).frame_slots).unwrap_or(0);
        for i in 0..frame_slots {
            let td = &mut (*dec).frame_thread_data[i];
            if !td.command_buffer.event.is_null() {
                Event::close(td.command_buffer.event);
            }
            td.command_buffer.event = std::ptr::null_mut();
        }
        for td in &mut (*dec).output_thread_data {
            if !td.command_buffer.event.is_null() {
                Event::close(td.command_buffer.event);
            }
            td.command_buffer.event = std::ptr::null_mut();
        }

        let memory = (*dec).memory;
        if !memory.is_null() {
            if let Some(device) = &device {
                log_destroy_step(device, available_cb, available_priv, "GPU thread joined, buffers not yet released");
                // The buffers the CPU maps first, the default-heap buffers the CPU never sees last: one line
                // after each memory type.
                for kind in [DeviceUpload, HostRW, UploadUAV, ReadBack, DeviceOnlyConst, DeviceOnly] {
                    let (bytes, count) = (*memory).release_type(kind);
                    if count == 0 {
                        continue;
                    }
                    let mut step = String::new();
                    let _ = write!(
                        step,
                        "released {}, {count} buffers, {:.1} MiB",
                        memory_type_name(kind),
                        to_mib(bytes as u64)
                    );
                    log_destroy_step(device, available_cb, available_priv, &step);
                }
            }
            (*memory).release();
            (*dec).memory = std::ptr::null_mut();
            drop(Box::from_raw(memory));
        }

        release_interfaces(dec);
        // The decoder itself: the queues, the counters of the timing run (-gputimedetail), the struct.
        // Every interface it held is released above, in that order.
        drop(Box::from_raw(dec));

        if let Some(device) = &device {
            log_destroy_step(device, available_cb, available_priv, "end, command queue, list and allocators released");
        }
    }
}

/// `av1_destroy_gpu_decoder` (`dx/av1_core.h`).
///
/// # Safety
/// `dec` is null or a decoder of [`av1_create_gpu_decoder`]; nothing uses it afterwards.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn av1_destroy_gpu_decoder(dec: *mut Av1Core) {
    if dec.is_null() {
        return;
    }
    // SAFETY: the caller's contract.
    unsafe { destroy(dec, true) };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_bounded_line_cuts_like_snprintf() {
        let mut line = BoundedLine::new(8);
        line.push("abc");
        assert!(!line.full());
        line.push("defghij");
        assert_eq!(line.text, "abcdefg");
        assert!(line.full());
    }

    /// The margin of the budget guard is a `size_t` expression in the header, which the bindings do not
    /// carry over.
    #[test]
    fn the_margin_is_that_of_the_header() {
        let header = concat!(env!("CARGO_MANIFEST_DIR"), "/../../libav1/aom/aom_decoder.h");
        let text = std::fs::read_to_string(header).expect("aom_decoder.h");
        assert!(text.contains("#define AV1_MEMORY_GUARD_MARGIN ((size_t)48 << 20)"));
        assert_eq!(MEMORY_GUARD_MARGIN, 48 << 20);
    }

    #[test]
    fn slots_of_a_configuration() {
        // SAFETY: a configuration is numbers and pointers.
        let mut cfg: aom_codec_dec_cfg_t = unsafe { std::mem::zeroed() };
        assert_eq!(frame_slots_from_cfg(&cfg), 2);
        for (asked, slots) in [(1, 2), (3, 3), (4, 4), (9, 4), (-1, 2)] {
            cfg.frame_slots = asked;
            assert_eq!(frame_slots_from_cfg(&cfg), slots);
        }
        cfg.frame_slots = 3;
        let rcfg = ResourceConfig::from_cfg(&cfg);
        assert_eq!((rcfg.ref_fb_count, rcfg.low_memory), (11, 1));
        cfg.low_memory = 8; // AV1_LOW_MEMORY_NONE
        let rcfg = ResourceConfig::from_cfg(&cfg);
        assert_eq!((rcfg.ref_fb_count, rcfg.low_memory), (13, 0));
    }
}
