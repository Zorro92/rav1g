// SPDX-License-Identifier: BSD-2-Clause
//
// This source code is subject to the terms of the BSD 2 Clause License and the Alliance for Open Media
// Patent License 1.0. If the BSD 2 Clause License was not distributed with this source code in the LICENSE
// file, you can obtain it at www.aomedia.org/license/software. If the Alliance for Open Media Patent
// License 1.0 was not distributed with this source code in the PATENTS file, you can obtain it at
// www.aomedia.org/license/patent.

//! The prediction stages: the shader that writes the block records from the mode info and the lists of
//! the tiles, inter prediction in its passes, the reconstruction of blocks whose residuals are added on
//! their own, intra prediction along its wavefront, and the borders of the finished frame. From
//! `av1_prediction_gen_blocks`, `av1_prediction_run_all` and `av1_inter_ext_borders` of
//! `libav1/dx/prediction.cpp`.

use std::ffi::c_int;
use std::sync::atomic::{AtomicI32, Ordering};

use super::core::{
    alloc_constants, buffer, commands, perf_gpu_mark, perf_inter_begin, perf_inter_mark, pipeline, stage_dump_active,
    stage_dump_data, stage_dump_fb, stage_dump_mode_info,
};
use super::frame::joined_runs;
use super::gpu_pred_lists::{self, DispatchKind, FrameLayout, Recording};
use super::intra_v2;
use super::stage_dump::stage_dump_gpu_records;
use super::tables::{
    BILINEAR_FILTERS, INTER_BLOCK_HEIGHT_LUT, INTER_BLOCK_WIDTH_LUT, INTRA_MODE_SHADER_PARAMS, OBMC_MASK,
    QUANT_DIST_LOOKUP_TABLE, QUANT_DIST_WEIGHT, SM_WEIGHT_ARRAYS, SUB_PEL_FILTERS_4, SUB_PEL_FILTERS_4SMOOTH,
    SUB_PEL_FILTERS_8, SUB_PEL_FILTERS_8SHARP, SUB_PEL_FILTERS_8SMOOTH,
};
use super::types::{
    Av1Core, CasualInter, CompoundDiffUv, CompoundDiffUvGlobalWarp, CompoundGlobalWarp, CompoundMasked, ComputeShader,
    Inter2x2ArrOffset, InterCountsAll, InterFast2D, InterFastClasses, InterFastLists, InterSizesAllCommon, IntraSizes,
    IntraTypeCount, ObmcAbove, ObmcLeft, ReconBlockOffset, ReconstructBlockSizes, ScaleFactor, Warp,
    av1_frame_thread_data, av1_perf_inter_tile, av1_stage_dump_region,
};
use crate::gpu::{BufferRef, Commands};
use crate::sys::{
    ALTREF_FRAME, AV1_COMMON, AV1_INTER_FAST_COMPOUND_ORIGINAL, AV1_INTER_FAST_SINGLE_ORIGINAL,
    AV1_PERF_GPU_EXT_BORDERS, AV1_PERF_GPU_GEN_BLOCKS, AV1_PERF_GPU_INTER, AV1_PERF_GPU_INTRA, AV1_PERF_GPU_RECON,
    AV1_PERF_INTER_CHROMA_2X2, AV1_PERF_INTER_COMP_AVG, AV1_PERF_INTER_COMP_AVG_FAST, AV1_PERF_INTER_COMP_DIFF_UV,
    AV1_PERF_INTER_COMP_DIFF_Y, AV1_PERF_INTER_COMP_MASKED, AV1_PERF_INTER_COMP_WARP, AV1_PERF_INTER_COMP_WARP_DIFF_UV,
    AV1_PERF_INTER_LISTS, AV1_PERF_INTER_OBMC_ABOVE, AV1_PERF_INTER_OBMC_LEFT, AV1_PERF_INTER_RECON,
    AV1_PERF_INTER_SECTION_COUNT, AV1_PERF_INTER_SINGLE, AV1_PERF_INTER_SINGLE_COPY, AV1_PERF_INTER_SYNC0,
    AV1_PERF_INTER_SYNC1, AV1_PERF_INTER_WARP, AV1Decoder, EXTREF_FRAME, HwFrameBuffer, INVALID_IDX, LAST_FRAME,
    MAX_FRAME_DISTANCE, OrderHintInfo, RefCntBuffer, SCALE_SUBPEL_BITS, WarpedMotionParams, av1_perf_inter_blocks,
    av1_perf_inter_detail, av1_perf_inter_work, av1_tile_data,
};

const SIZES: usize = InterSizesAllCommon as usize;

fn plane_words(buffer: &HwFrameBuffer) -> [[c_int; 4]; 3] {
    buffer.planes.map(|plane| [plane.stride, plane.offset, plane.res_stride, plane.res_offset])
}

// ---------------------------------------------------------------------------------------------------------
// The block records.
// ---------------------------------------------------------------------------------------------------------

/// `get_relative_dist` of the C++: the distance of two order hints, which count modulo a power of two.
fn get_relative_dist(oh: &OrderHintInfo, a: c_int, b: c_int) -> c_int {
    if oh.enable_order_hint == 0 {
        return 0;
    }
    let bits = oh.order_hint_bits_minus_1 + 1;
    assert!(bits >= 1);
    assert!(a >= 0 && a < (1 << bits));
    assert!(b >= 0 && b < (1 << bits));
    let diff = a - b;
    let m = 1 << (bits - 1);
    (diff & (m - 1)) - (diff & m)
}

/// `get_ref_frame_buf` of `av1/common/onyxc_int.h`: the buffer of a reference of the frame, or null.
///
/// # Safety
/// `cm` is the frame header.
unsafe fn get_ref_frame_buf(cm: &AV1_COMMON, ref_frame: c_int) -> *const RefCntBuffer {
    let map_idx = if (LAST_FRAME..=EXTREF_FRAME).contains(&ref_frame) {
        cm.remapped_ref_idx[(ref_frame - LAST_FRAME) as usize]
    } else {
        INVALID_IDX
    };
    if map_idx != INVALID_IDX { cm.ref_frame_map[map_idx as usize] } else { std::ptr::null() }
}

/// The weights of a compound prediction that weighs its two references by their distance to the frame,
/// as the specification derives them, packed into one word (forward weight, backward weight << 4).
fn dist_wtd_weights(d0: c_int, d1: c_int) -> c_int {
    let order = usize::from(d0 <= d1);
    let lookup = |i: usize, order: usize| QUANT_DIST_LOOKUP_TABLE[i * 2 + order];
    let (fwd_offset, bck_offset) = if d0 == 0 || d1 == 0 {
        (lookup(3, order), lookup(3, 1 - order))
    } else {
        let mut i = 0;
        while i < 3 {
            let c0 = QUANT_DIST_WEIGHT[i * 2 + order];
            let c1 = QUANT_DIST_WEIGHT[i * 2 + (1 - order)];
            let d0_c0 = d0 * c0;
            let d1_c1 = d1 * c1;
            if (d0 > d1 && d0_c0 < d1_c1) || (d0 <= d1 && d0_c0 > d1_c1) {
                break;
            }
            i += 1;
        }
        (lookup(i, order), lookup(i, 1 - order))
    };
    fwd_offset | (bck_offset << 4)
}

/// A warp model in the constants of `gen_pred_blocks.hlsl`.
#[repr(C)]
struct WarpParams {
    params: WarpedMotionParams,
    pad: c_int,
}

/// The constants of `gen_pred_blocks.hlsl`.
#[repr(C)]
struct GenBlockData {
    mi_cols: c_int,
    mi_rows: c_int,
    mi_stride: c_int,
    mi_addr_base: u32,
    iter_grid_stride: c_int,
    iter_grid_offset_uv: c_int,
    iter_grid_stride_uv: c_int,
    disable_edge_filter: c_int,
    force_integet_mv: c_int,
    mi_grid_entry: u32,
    mi_grid_last: u32,
    reserved: c_int,
    wedge_offsets: [[c_int; 4]; 22],
    dist_wtd: [[c_int; 4]; 8 * 8],
    lossless_seg: [[c_int; 4]; 8],

    global_warp: [[c_int; 4]; 8],
    wm_params: [WarpParams; 8],
}

/// The records `gen_pred_blocks` writes for a frame into `prediction_blocks` (with the lists by motion
/// vector class of -interfast) and into `prediction_blocks_warp`: the counts of the lists of its tiles,
/// which [`prediction_gen_blocks`] turns into offsets. For the buffers, before the frame is recorded
/// (`stage_buffers.rs`).
///
/// # Safety
/// `tiles` are the tiles of the frame on `td`, parsed, with their block maps as the parse left them.
pub(crate) unsafe fn pred_block_counts(td: &av1_frame_thread_data, tiles: &[av1_tile_data]) -> (c_int, c_int) {
    // SAFETY: the caller's contract: the lists of a tile are in the block map of its slot.
    unsafe {
        let iters = |tile: &av1_tile_data| (-1).max(tile.gen_intra_iter_y).max(tile.gen_intra_iter_uv);
        let types = |iters: c_int| InterCountsAll + ReconstructBlockSizes + (iters + 1) * IntraTypeCount;
        let pred_block_types = types(tiles.iter().map(iters).max().unwrap_or(-1));
        let gpu_lists = tiles.first().is_some_and(|tile| tile.gpu_pred_lists != 0);
        let first = if gpu_lists { InterCountsAll + ReconstructBlockSizes } else { 0 };
        let mut blocks = 0;
        let mut warp = 0;
        for tile in tiles {
            // What the loops of prediction_gen_blocks take: the lists below the tile's gen_pred_map_max.
            let lists = (types(iters(tile)) + 1).min(pred_block_types + 1);
            for it in first..lists {
                blocks += *tile.gen_block_map.offset(it as isize);
            }
            if gpu_lists {
                blocks += tile.pred_block_bound;
                warp += tile.pred_warp_bound;
                continue;
            }
            for it in 0..=SIZES {
                warp += *tile.gen_block_map_wrp.add(it);
            }
            if td.inter_fast != 0 && !tile.gen_block_map_fast.is_null() {
                for it in 0..=InterFastLists as usize {
                    blocks += *tile.gen_block_map_fast.add(it);
                }
            }
        }
        (blocks, warp)
    }
}

/// `av1_prediction_gen_blocks` of the C++: turns the counts of the tiles' lists into where each list
/// starts in the block records of the frame, and records the shader that writes the records.
///
/// # Safety
/// `dec` is the decoder while it records a frame whose tiles are decoded; `pbi` is its entropy decoder.
pub(super) unsafe fn prediction_gen_blocks<'a>(pbi: *mut AV1Decoder, dec: *mut Av1Core) -> Option<Recording<'a>> {
    // SAFETY: the caller's contract: the frame data, its tiles and their block maps, the frame header and
    // the buffers and pipelines of the decoder are live, and only this thread records.
    unsafe {
        let cm = &raw mut (*pbi).common;
        let xd = &(*pbi).mb;
        let td = &mut *(*dec).curr_frame_data;

        let constants = alloc_constants::<GenBlockData>(&mut td.command_buffer);
        let data = constants.data;
        (*data).mi_cols = (*cm).mi_cols;
        (*data).mi_rows = (*cm).mi_rows;
        (*data).mi_stride = (*cm).mi_stride;
        // The grid: entries of four bytes, the place of a record in the pool above this base.
        (*data).mi_addr_base = crate::sys::MI_GRID_BASE;
        (*data).mi_grid_entry = crate::sys::MI_GRID_ENTRY_BYTES;
        // The last entry of the slot's grid: a load of the shader that is not in the grid reads it.
        (*data).mi_grid_last = ((*td.mode_info_grid).size / size_of::<crate::sys::MiGridEntry>() - 1) as u32;
        (*data).iter_grid_stride = td.iter_grid_stride;
        (*data).iter_grid_offset_uv = td.iter_grid_offset_uv;
        (*data).iter_grid_stride_uv = td.iter_grid_stride_uv;
        (*data).disable_edge_filter = c_int::from((*cm).seq_params.enable_intra_edge_filter == 0);
        (*data).force_integet_mv = c_int::from((*cm).cur_frame_force_integer_mv);
        for bs in 0..22 {
            (*data).wedge_offsets[bs][0] = (*dec).wedge_offsets[bs][0];
            (*data).wedge_offsets[bs][1] = (*dec).wedge_offsets[bs][1];
        }
        for i in 0..8 {
            (*data).lossless_seg[i][0] = xd.lossless[i];
        }
        for i in 0..7 {
            (*data).global_warp[i][0] = td.gm_warp[i + 1];
        }
        let order_hint_info = &(*cm).seq_params.order_hint_info;
        let cur_frame_index = (*(*cm).cur_frame).order_hint as c_int;
        let order_hint = |reference: usize| {
            let buf = get_ref_frame_buf(&*cm, reference as c_int + 1);
            if buf.is_null() { 0 } else { (*buf).order_hint as c_int }
        };
        for r1 in 0..7 {
            for r0 in 0..7 {
                let bck_frame_index = order_hint(r0);
                let fwd_frame_index = order_hint(r1);
                let d0 = get_relative_dist(order_hint_info, fwd_frame_index, cur_frame_index)
                    .abs()
                    .clamp(0, MAX_FRAME_DISTANCE as c_int);
                let d1 = get_relative_dist(order_hint_info, cur_frame_index, bck_frame_index)
                    .abs()
                    .clamp(0, MAX_FRAME_DISTANCE as c_int);
                (*data).dist_wtd[r0 + 8 * r1][0] = dist_wtd_weights(d0, d1);
            }
        }
        for i in 0..7 {
            // The model as it is, with the bytes between its fields.
            (&raw mut (*data).wm_params[i].params)
                .cast::<u8>()
                .copy_from_nonoverlapping(xd.global_motion.add(i + 1).cast::<u8>(), size_of::<WarpedMotionParams>());
        }

        let tile_count = td.tile_count as usize;
        let tiles = std::slice::from_raw_parts_mut(td.tile_data, tile_count);
        // The tile with the most wavefront iterations goes first: the stages read the starts of the lists
        // from the first tile, which then has all of them.
        let mut intra_iters = -1;
        let mut main_tile = 0;
        for (t, tile) in tiles.iter_mut().enumerate() {
            let tile_iters = (-1).max(tile.gen_intra_iter_y).max(tile.gen_intra_iter_uv);
            if tile_iters > intra_iters {
                intra_iters = tile_iters;
                main_tile = t;
            }
            tile.gen_pred_map_max = InterCountsAll + ReconstructBlockSizes + (tile_iters + 1) * IntraTypeCount + 1;
        }

        if main_tile != 0 {
            tiles.swap(0, main_tile);
            // What the tiles took of the pools goes with them.
            td.pieces.swap(0, main_tile);
        }

        // The block index lists of the tiles, one behind the other in the slot's buffer, which
        // stage_buffers::for_frame has made large enough: a tile's list starts where gen_pred_blocks is
        // told it does (mi_idx_base).
        {
            let list = &*td.gen_mi_block_indexes;
            let host = list.host_ptr.cast::<u32>();
            let mut base = 0usize;
            for tile in tiles.iter_mut() {
                let count = tile.gen_index_ptr.max(0) as usize;
                assert!(
                    (base + count) * size_of::<u32>() <= list.size,
                    "the block index list of the slot is too small"
                );
                host.add(base).copy_from_nonoverlapping(tile.gen_indexes.cast_const(), count);
                tile.gen_index_base = base as c_int;
                base += count;
            }
        }

        td.intra_iters = intra_iters;
        let gpu_layout = FrameLayout::for_frame(td, tiles);
        let inter_fast = gpu_pred_lists::inter_fast(td, tiles);
        let normal_lists = InterCountsAll + ReconstructBlockSizes;
        let pred_block_types = normal_lists + (intra_iters + 1) * IntraTypeCount;
        let first = if gpu_layout.is_some() { normal_lists } else { 0 };
        let mut offset = 0;
        for it in first..=pred_block_types {
            for tile in tiles.iter() {
                if it < tile.gen_pred_map_max {
                    let list = tile.gen_block_map.offset(it as isize);
                    let cnt = *list;
                    *list = offset;
                    offset += cnt;
                }
            }
        }
        let mut pred_block_total = offset;
        let mut pred_warp_count = 0;
        if gpu_layout.is_some() {
            // The intra offsets are relative to their own records. The scan adds their
            // count to the start of the fast lists and returns the actual output lengths.
            pred_block_total += tiles.iter().map(|tile| tile.pred_block_bound).sum::<c_int>();
            pred_warp_count = tiles.iter().map(|tile| tile.pred_warp_bound).sum();
        } else {
            for it in 0..=SIZES {
                for tile in tiles.iter() {
                    let list = tile.gen_block_map_wrp.add(it);
                    let cnt = *list;
                    *list = pred_warp_count;
                    pred_warp_count += cnt;
                }
            }
            // -interfast: these lists follow the intra records in the ordinary prediction buffer.
            if inter_fast {
                for it in 0..=InterFastLists as usize {
                    for tile in tiles.iter() {
                        if tile.gen_block_map_fast.is_null() {
                            continue;
                        }
                        let list = tile.gen_block_map_fast.add(it);
                        let cnt = *list;
                        *list = pred_block_total;
                        pred_block_total += cnt;
                    }
                }
            }
        }
        super::mem_stats::pred_blocks(dec, pred_block_total, pred_warp_count);
        debug_assert!(pred_block_total as usize * 16 <= (*(*dec).prediction_blocks).size);
        debug_assert!(pred_warp_count as usize * 48 <= (*(*dec).prediction_blocks_warp).size);

        let commands = commands(dec);
        let lib = (*dec).shader_lib;
        let mode_info = buffer((*dec).mode_info_pool).location();
        let mi_grid = buffer(td.mode_info_grid).location();
        let blocks = buffer((*dec).prediction_blocks);
        let warp_blocks = buffer((*dec).prediction_blocks_warp);

        // A tile's contiguous runs, in parse order. A run's first group continues the numbering
        // from the preceding run, even if the pieces of the pool do not follow each other.
        let dispatch_runs = |srt_slot: u32, srt_words: usize| {
            let mut group = 0;
            for (tile, run) in tiles
                .iter()
                .zip(&td.pieces)
                .flat_map(|(tile, pieces)| joined_runs(&pieces.mode_info).into_iter().map(move |run| (tile, run)))
            {
                let mut inline = [
                    run.count,
                    run.first,
                    tile.gen_index_base,
                    tile.mi_col_start,
                    tile.mi_row_start,
                    tile.gen_block_map_offset,
                    tile.gen_block_warp_offset,
                    tile.gen_block_fast_offset,
                    0,
                    0,
                    0,
                    0,
                ];
                if let Some(gpu) = gpu_layout {
                    inline[7] = c_int::from(inter_fast);
                    inline[8..].copy_from_slice(&[
                        group as c_int,
                        gpu.lists as c_int,
                        gpu.rank_offset() as c_int,
                        gpu.starts_offset() as c_int,
                    ]);
                }
                commands.set_inline_constants(srt_slot, &inline[..srt_words], 0);
                let groups = (run.count as u32).div_ceil(gpu_pred_lists::GROUP_SIZE);
                commands.dispatch(groups, 1, 1);
                group += groups;
            }
            if let Some(gpu) = gpu_layout {
                debug_assert_eq!(group, gpu.groups);
            }
        };
        let gpu_lists = gpu_layout.map(|gpu| Recording::new(dec, gpu));
        if let Some(gpu) = &gpu_lists {
            Recording::mark_grid(dec);
            let shader = pipeline(&raw const (*lib).shader_pred_list_count);
            commands.set_layout(shader.layout());
            commands.set_pipeline(shader);
            commands.bind_read(0, mode_info);
            commands.bind_read(3, mi_grid);
            commands.bind_write(4, gpu.scratch().location());
            commands.bind_constants(5, constants.location);
            dispatch_runs(6, 12);
            gpu.scan(dec, offset as u32);
        }
        // -dumpstages: the mode info, where the first stage that reads it starts.
        stage_dump_mode_info(dec, cm);
        let (shader, srt_words) = if gpu_lists.is_some() {
            (pipeline(&raw const (*lib).shader_gen_pred_blocks_gpu), 12)
        } else if inter_fast {
            (pipeline(&raw const (*lib).shader_gen_pred_blocks_fast), 8)
        } else {
            (pipeline(&raw const (*lib).shader_gen_pred_blocks), 7)
        };
        commands.set_layout(shader.layout());
        commands.set_pipeline(shader);
        commands.bind_read(0, mode_info);
        commands.bind_read(1, buffer(td.gen_mi_block_indexes).location());
        commands.bind_read(2, buffer(td.gen_block_map).location());
        commands.bind_read(3, mi_grid);
        commands.bind_read(4, buffer(td.gen_intra_inter_grid).location());
        let root = if let Some(gpu) = &gpu_lists {
            commands.bind_read(5, gpu.scratch().location());
            6
        } else {
            5
        };
        commands.bind_write(root, blocks.location());
        commands.bind_write(root + 1, warp_blocks.location());
        commands.bind_constants(root + 2, constants.location);
        dispatch_runs(root + 3, srt_words);
        commands.write_barrier2(blocks, warp_blocks);
        perf_gpu_mark(dec, AV1_PERF_GPU_GEN_BLOCKS);
        // -dumpstages: the block records gen_pred_blocks has just written, as the prediction stages read
        // them.
        if stage_dump_active(dec) {
            for (i, (stage, object, stride, count)) in [
                (c"pred_blocks", (*dec).prediction_blocks, 16, pred_block_total),
                (c"pred_blocks_warp", (*dec).prediction_blocks_warp, 48, pred_warp_count),
            ]
            .into_iter()
            .enumerate()
            {
                let size = (count as usize * stride).min((*object).size);
                let region = av1_stage_dump_region { src: (*object).dev, offset: 0, size, uav: 1 };
                if let Some(gpu) = gpu_layout {
                    let counts = av1_stage_dump_region {
                        src: (*(*dec).gpu_pred_scratch).dev,
                        offset: gpu.starts_offset() as usize + i * 4,
                        size: 4,
                        uav: 0,
                    };
                    stage_dump_gpu_records(dec, stage, counts, region, stride);
                } else {
                    let layout =
                        format!("buffer={} record={stride} count={}\n", stage.to_string_lossy(), size / stride);
                    stage_dump_data(dec, stage, &layout, 160, &[region]);
                }
            }
        }
        gpu_lists
    }
}

// ---------------------------------------------------------------------------------------------------------
// Inter prediction, reconstruction, intra prediction.
// ---------------------------------------------------------------------------------------------------------

/// A reference plane in the constants of the inter shaders (`RefPlaneInfo` of `aom_scale/yv12config.h`).
#[repr(C)]
struct RefPlaneInfo {
    stride: c_int,
    offset: c_int,
    width: c_int,
    height: c_int,
}

/// A global motion model in the constants of the inter shaders.
#[repr(C)]
struct GlobalMotionWarp {
    is_warp: c_int,
    reserved: c_int,
    mat: [c_int; 6],
    alpha: c_int,
    beta: c_int,
    delta: c_int,
    gamma: c_int,
}

/// The constants of the inter shaders.
#[repr(C)]
struct InterData {
    planes: [[c_int; 4]; 3],
    refplanes: [RefPlaneInfo; 3 * 7],
    dims: [[c_int; 4]; 2],
    pixel_max: [c_int; 4],
    kernels: [[c_int; 8]; 8 * 16],
    scale: [ScaleFactor; 8],
    obmc_mask: [c_int; (1 + 1 + 2 + 4 + 8) * 4],
    warp: [GlobalMotionWarp; 7],
}

/// The constants of the intra shaders and of the reconstruction.
#[repr(C)]
struct IntraData {
    planes: [[c_int; 4]; 3],
    flags: [c_int; 4],
    filter: [[[c_int; 8]; 8]; 5],
    mode_params_lut: [[[c_int; 4]; 7]; 16],
    sm_weight_arrays: [[c_int; 4]; 128],
}

static INTER_LEGACY: AtomicI32 = AtomicI32::new(0);

/// `av1_set_inter_legacy` (`dx/av1_perf.h`).
#[unsafe(no_mangle)]
pub extern "C" fn av1_set_inter_legacy(enable: c_int) {
    INTER_LEGACY.store(enable, Ordering::Relaxed);
}

// The main intra path uses DXC's optimized build by default. -intraod selects the baseline kernel.
static INTRA_OPT: AtomicI32 = AtomicI32::new(1);

/// `av1_set_intra_opt` (`dx/av1_perf.h`).
#[unsafe(no_mangle)]
pub extern "C" fn av1_set_intra_opt(enable: c_int) {
    INTRA_OPT.store(enable, Ordering::Relaxed);
}

// Test flag -intraedgefix: intra_main.hlsl (8-bit) stops the taps of the above edge filter at filter_count,
// as intra_main_hbd.hlsl does, instead of at above_count. The output is the same (see the shader). Off
// unless av1_set_intra_edge_fix(1) was called.
static INTRA_EDGE_FIX: AtomicI32 = AtomicI32::new(0);

/// `av1_set_intra_edge_fix` (`dx/av1_perf.h`).
#[unsafe(no_mangle)]
pub extern "C" fn av1_set_intra_edge_fix(enable: c_int) {
    INTRA_EDGE_FIX.store(enable, Ordering::Relaxed);
}

/// Inter breakdown of the timing run (-gputimedetail, dx/av1_perf.h): what the recording notes about the
/// dispatches of the inter stage and of the reconstruction. Without a record (`d` null) every call returns
/// at once and the command list is the one it always was.
struct InterDetailRec<'a> {
    dec: *mut Av1Core,
    d: *mut av1_perf_inter_detail,
    commands: Commands<'a>,
    frame_buffers: BufferRef<'a>,
    /// Reference bytes one thread of a section's shader loads.
    thread_bytes: [c_int; AV1_PERF_INTER_SECTION_COUNT as usize],
}

impl InterDetailRec<'_> {
    /// Estimate of reference bytes from rows per thread and reference times bytes loaded per row.
    /// The unscaled size lists use the two-pass load count, an upper bound
    /// when the shader skips an identity phase. The opt-in fast lists give
    /// the motion class, so their estimates account for skipped passes.
    /// For scaled references use the largest step of the frame's references.
    ///
    /// # Safety
    /// `self.d` is null or the breakdown of the frame.
    unsafe fn init(&mut self, td: &av1_frame_thread_data) {
        // SAFETY: the caller's contract.
        let Some(d) = (unsafe { self.d.as_mut() }) else { return };
        let hbd = td.is_hbd != 0;
        d.scaled = td.scale_enable;
        d.bytes_per_sample = if hbd { 2 } else { 1 };
        let row = if hbd { 24 } else { 16 }; // filter_line(_hbd): Load4 (+ Load2), four samples of a row
        let thread_bytes = &mut self.thread_bytes;
        let mut set = |section: c_int, bytes: c_int| thread_bytes[section as usize] = bytes;
        set(AV1_PERF_INTER_WARP, 11 * row);
        if td.scale_enable == 0 {
            set(AV1_PERF_INTER_SINGLE, 11 * row); // a 4x4: 11 rows
            set(AV1_PERF_INTER_COMP_AVG, 2 * 9 * row); // a 4x2: 9 rows of each reference
            set(AV1_PERF_INTER_COMP_DIFF_Y, 2 * 9 * row);
            set(AV1_PERF_INTER_COMP_MASKED, 2 * 9 * row);
            set(AV1_PERF_INTER_COMP_DIFF_UV, 2 * 9 * row);
            set(AV1_PERF_INTER_CHROMA_2X2, 9 * if hbd { 20 } else { 12 }); // a 2x2: 9 rows of two samples
            set(AV1_PERF_INTER_OBMC_ABOVE, 11 * row);
            set(AV1_PERF_INTER_OBMC_LEFT, 11 * row);
            // -interfast (inter_fast.hlsl, inter_compound_fast.hlsl): rows by the class of the reference, of
            // four samples (inter_fast_row: one Load2, two for 16-bit samples) where the class has no
            // horizontal pass.
            let row4 = if hbd { 16 } else { 8 };
            let rows4x4 = [4 * row4, 4 * row, 11 * row4, 11 * row];
            let rows4x2 = [2 * row4, 2 * row, 9 * row4, 9 * row];
            // With the original shader on the lists (av1_set_inter_fast, the lists without the kernels)
            // every class loads what the last one does.
            let single_original = (td.inter_fast & AV1_INTER_FAST_SINGLE_ORIGINAL) != 0;
            let compound_original = (td.inter_fast & AV1_INTER_FAST_COMPOUND_ORIGINAL) != 0;
            for c in 0..InterFastClasses {
                set(AV1_PERF_INTER_SINGLE_COPY + c, rows4x4[if single_original { InterFast2D } else { c } as usize]);
            }
            for c in 0..InterFastClasses * InterFastClasses {
                set(
                    AV1_PERF_INTER_COMP_AVG_FAST + c,
                    if compound_original {
                        2 * rows4x2[InterFast2D as usize]
                    } else {
                        rows4x2[(c & 3) as usize] + rows4x2[(c >> 2) as usize]
                    },
                );
            }
        } else {
            let mut step = 1 << SCALE_SUBPEL_BITS;
            for i in LAST_FRAME..=ALTREF_FRAME {
                step = step.max(td.scale_factors[i as usize].y_step);
            }
            let col = if hbd { 20 } else { 12 }; // one sample of a row: Load4 + Load, or Load3
            let lines4 = 20.min(8 + ((3 * step) >> SCALE_SUBPEL_BITS)); // a column of a 4x4
            let lines2 = 16.min(8 + (step >> SCALE_SUBPEL_BITS)); // a column of a 2x2
            set(AV1_PERF_INTER_SINGLE, lines4 * col);
            set(AV1_PERF_INTER_COMP_AVG, 2 * lines4 * col);
            set(AV1_PERF_INTER_COMP_DIFF_Y, 2 * lines4 * col);
            set(AV1_PERF_INTER_COMP_MASKED, 2 * lines4 * col);
            set(AV1_PERF_INTER_COMP_DIFF_UV, 2 * lines4 * col);
            set(AV1_PERF_INTER_CHROMA_2X2, lines2 * col);
            set(AV1_PERF_INTER_OBMC_ABOVE, lines4 * col);
            set(AV1_PERF_INTER_OBMC_LEFT, lines4 * col);
        }
        // inter_scale_warp_compound, with or without scaled references: a column of a 4x4, 11 rows of a
        // warped reference (a reference that is not warped: the rows of the scaled form; 11 is taken for
        // both).
        set(AV1_PERF_INTER_COMP_WARP, 2 * 11 * if hbd { 20 } else { 12 });
        set(AV1_PERF_INTER_COMP_WARP_DIFF_UV, 2 * 11 * if hbd { 20 } else { 12 });
    }

    /// Behind a dispatch of `records` blocks of `width` x `height` samples, `wi_count` threads with work.
    ///
    /// # Safety
    /// `self.d` is null or the breakdown of the frame, `self.dec` the decoder that records it.
    #[expect(clippy::too_many_arguments, reason = "the arguments of the C++ function")]
    #[inline]
    unsafe fn dispatch(
        &mut self,
        stage: c_int,
        section: c_int,
        list: c_int,
        width: c_int,
        height: c_int,
        records: c_int,
        wi_count: u32,
    ) {
        // SAFETY: the caller's contract.
        let Some(d) = (unsafe { self.d.as_mut() }) else { return };
        let s = &mut d.sections[section as usize];
        let groups = ((wi_count + 63) >> 6) as c_int;
        s.dispatches += 1;
        s.groups += groups;
        s.threads += i64::from(wi_count);
        s.records += i64::from(records);
        s.pixels += i64::from(records) * i64::from(width) * i64::from(height);
        s.loaded_bytes += i64::from(wi_count) * i64::from(self.thread_bytes[section as usize]);
        if (0..AV1_PERF_INTER_LISTS).contains(&list) {
            let l = &mut s.lists[list as usize];
            l.width = width;
            l.height = height;
            l.records += records;
            l.groups += groups;
            l.threads += wi_count as c_int;
        }
        if d.mode == 2 {
            // The dispatch alone: nothing behind the barrier starts before it is done, and the timestamp
            // behind the barrier is its end. A barrier only orders, so the frame is the same.
            self.commands.write_barrier(self.frame_buffers);
            d.barriers_added += 1;
            // SAFETY: the caller's contract.
            unsafe { perf_inter_mark(self.dec, stage, section, list) };
        }
    }

    /// Behind the last dispatch of a section of the inter stage (mode 1: its timestamp).
    ///
    /// # Safety
    /// As for [`Self::dispatch`].
    unsafe fn section(&mut self, section: c_int) {
        // SAFETY: the caller's contract.
        unsafe {
            if let Some(d) = self.d.as_ref()
                && d.mode == 1
                && d.sections[section as usize].dispatches != 0
            {
                perf_inter_mark(self.dec, AV1_PERF_GPU_INTER, section, -1);
            }
        }
    }

    /// Behind a barrier of the inter stage.
    ///
    /// # Safety
    /// As for [`Self::dispatch`].
    unsafe fn sync(&mut self, section: c_int) {
        if !self.d.is_null() {
            // SAFETY: the caller's contract.
            unsafe { perf_inter_mark(self.dec, AV1_PERF_GPU_INTER, section, -1) };
        }
    }

    /// The counters of the tile threads, added up over the tiles of the frame.
    ///
    /// # Safety
    /// `self.d` is null or the breakdown of the frame.
    unsafe fn add_tiles(&mut self, tiles: &[av1_perf_inter_tile]) {
        // The counters are all 64-bit numbers.
        const WORK: usize = size_of::<av1_perf_inter_work>() / size_of::<i64>();
        const BLOCKS: usize = size_of::<av1_perf_inter_blocks>() / size_of::<i64>();
        // SAFETY: the caller's contract; the structs are WORK and BLOCKS 64-bit counters.
        unsafe {
            if self.d.is_null() {
                return;
            }
            let d = &mut *self.d;
            for tile in tiles {
                for (section, work) in d.sections.iter_mut().zip(&tile.work) {
                    let src = (&raw const *work).cast::<[i64; WORK]>();
                    let dst = (&raw mut section.work).cast::<[i64; WORK]>();
                    for i in 0..WORK {
                        (*dst)[i] += (*src)[i];
                    }
                }
                let src = (&raw const tile.blocks).cast::<[i64; BLOCKS]>();
                let dst = (&raw mut d.blocks).cast::<[i64; BLOCKS]>();
                for i in 0..BLOCKS {
                    (*dst)[i] += (*src)[i];
                }
            }
        }
    }
}

/// The dispatches of the inter stage: the pipeline of the last one, so that it is only set when it
/// changes, and the breakdown.
struct InterPass<'a> {
    commands: Commands<'a>,
    shader: *const ComputeShader,
    detail: InterDetailRec<'a>,
    gpu: Option<Recording<'a>>,
}

struct ListWork {
    width_log2: u32,
    height_log2: u32,
    threads_per_record: u32,
    width: c_int,
    height: c_int,
}

impl ListWork {
    fn new(width_log2: u32, height_log2: u32, extra_y: u32, threads: u32) -> Self {
        Self {
            width_log2,
            height_log2: height_log2 + extra_y,
            threads_per_record: threads << (width_log2 + height_log2 + extra_y),
            width: 4 << width_log2,
            height: 4 << height_log2,
        }
    }

    fn inter_size(i: usize, extra_y: u32, threads: u32) -> Self {
        Self::new(INTER_BLOCK_WIDTH_LUT[i] as u32, INTER_BLOCK_HEIGHT_LUT[i] as u32, extra_y, threads)
    }
}

impl InterPass<'_> {
    /// One batch of lists with one shader. The GPU path compacts nonempty lists into a single
    /// ExecuteIndirect call. Both paths use the same group sizes and root constants.
    ///
    /// # Safety
    /// `shader` is a live pipeline and `offsets` contains `lists` range starts and their sentinel.
    unsafe fn lists(
        &mut self,
        shader: *const ComputeShader,
        offsets: *const c_int,
        first: u32,
        lists: usize,
        section: c_int,
        work: impl Fn(usize) -> ListWork,
    ) {
        // SAFETY: the caller's contract.
        unsafe {
            if let Some(gpu) = &mut self.gpu {
                if self.shader != shader {
                    self.shader = shader;
                    self.commands.set_pipeline(pipeline(shader));
                }
                gpu.batch(
                    self.commands,
                    DispatchKind::Inter,
                    0,
                    (0..lists).map(|i| {
                        let w = work(i);
                        [first + i as u32, w.threads_per_record, w.width_log2, w.height_log2]
                    }),
                );
            } else {
                for i in 0..lists {
                    let (offset, count) = list_range(offsets, i);
                    if count == 0 {
                        continue;
                    }
                    if self.shader != shader {
                        self.shader = shader;
                        self.commands.set_pipeline(pipeline(shader));
                    }
                    let w = work(i);
                    let wi_count = w.threads_per_record * count as u32;
                    self.commands.set_inline_constants(7, &[wi_count, offset as u32, w.width_log2, w.height_log2], 0);
                    self.commands.dispatch(wi_count.div_ceil(64), 1, 1);
                    self.detail.dispatch(AV1_PERF_GPU_INTER, section, i as c_int, w.width, w.height, count, wi_count);
                }
                self.detail.section(section);
            }
        }
    }
}

/// The start of a list in the block records and the number of its blocks, from the starts of the lists
/// of the first tile.
///
/// # Safety
/// `offsets` has an entry at `i` and one behind it.
#[inline]
unsafe fn list_range(offsets: *const c_int, i: usize) -> (c_int, c_int) {
    // SAFETY: the caller's contract.
    unsafe {
        let offset = *offsets.add(i);
        (offset, *offsets.add(i + 1) - offset)
    }
}

/// `av1_prediction_run_all` of the C++: records inter prediction, reconstruction and intra prediction
/// for the frame of the current command list.
///
/// # Safety
/// `dec` is the decoder while it records a frame whose block records are written
/// ([`prediction_gen_blocks`]); `cm` is the frame's.
pub(super) unsafe fn prediction_run_all(dec: *mut Av1Core, cm: *mut AV1_COMMON, gpu: Option<Recording<'_>>) {
    // SAFETY: the caller's contract: the frame data, its tiles and their block maps, the reference frame
    // buffers of a frame with inter blocks, and the buffers and pipelines of the decoder are live, and
    // only this thread records.
    unsafe {
        let td = &mut *(*dec).curr_frame_data;
        let tile_count = td.tile_count;
        let tiles = td.tile_data;
        // The starts of the lists of the frame: those of its first tile.
        let block_map = (*tiles).gen_block_map.cast_const();
        let block_map_wrp = (*tiles).gen_block_map_wrp.cast_const();
        let block_map_fast = (*tiles).gen_block_map_fast.cast_const();

        // 1 Prepare command buffer and common data
        let shaders = td.shaders;
        let lib = (*dec).shader_lib;
        let commands = commands(dec);
        let frame_buffers = buffer((*dec).frame_buffer_pool);
        let blocks = buffer((*dec).prediction_blocks);
        let residuals = buffer((*dec).idct_residuals);
        let mask_lut = buffer((*dec).inter_mask_lut);

        let do_inter = std::slice::from_raw_parts(tiles, tile_count as usize).iter().any(|tile| tile.have_inter != 0);

        // -gputimedetail: the inter breakdown of the frame, null without it.
        let mut detail = InterDetailRec {
            dec,
            d: if gpu.is_some() { std::ptr::null_mut() } else { perf_inter_begin(dec) },
            commands,
            frame_buffers,
            thread_bytes: [0; AV1_PERF_INTER_SECTION_COUNT as usize],
        };
        detail.init(td);
        if let Some(d) = detail.d.as_mut() {
            d.has_inter = c_int::from(do_inter);
            let counters = &td.perf_inter_tiles;
            detail.add_tiles(&counters[..counters.len().min(usize::try_from(tile_count).unwrap_or(0))]);
        }
        let mut pass = InterPass { commands, shader: std::ptr::null(), detail, gpu };
        if do_inter {
            let constants = alloc_constants::<InterData>(&mut td.command_buffer);
            let inter_data = constants.data;
            (*inter_data).planes = plane_words(&*td.frame_buffer);
            (*inter_data).scale = td.scale_factors;
            for i in 0..7 {
                let reference = &*td.refs[i];
                for p in 0..3 {
                    let plane = &raw mut (*inter_data).refplanes[i * 3 + p];
                    (*plane).offset = reference.planes[p].offset;
                    (*plane).stride = reference.planes[p].stride;
                    (*plane).width = if p == 0 { reference.y_crop_width - 1 } else { reference.uv_crop_width - 1 };
                    (*plane).height = if p == 0 { reference.y_crop_height - 1 } else { reference.uv_crop_height - 1 };
                }
            }
            // Read back from the record, as the C++ did.
            (*inter_data).dims[0][0] = (*inter_data).refplanes[0].width;
            (*inter_data).dims[0][1] = (*inter_data).refplanes[0].height;
            (*inter_data).dims[1][0] = (*inter_data).refplanes[1].width;
            (*inter_data).dims[1][1] = (*inter_data).refplanes[1].height;

            (&raw mut (*inter_data).kernels).write_bytes(0, 1);
            // The 4-tap filters first.
            let kernels: [&[i16; 128]; 6] = [
                &SUB_PEL_FILTERS_4,
                &SUB_PEL_FILTERS_4SMOOTH,
                &BILINEAR_FILTERS,
                &SUB_PEL_FILTERS_8,
                &SUB_PEL_FILTERS_8SMOOTH,
                &SUB_PEL_FILTERS_8SHARP,
            ];
            for (i, kernel) in kernels.iter().enumerate() {
                for j in 0..16 {
                    for k in 0..8 {
                        (*inter_data).kernels[i * 16 + j][k] = c_int::from(kernel[j * 8 + k]);
                    }
                }
            }
            (*inter_data).obmc_mask = OBMC_MASK;

            let mut enable_gm_warp = false;
            for i in 0..7 {
                let wm = &(*cm).global_motion[i + 1];
                let is_gmwarp = td.gm_warp[i + 1]; // not for a scaled reference: av1_setup_frame()
                enable_gm_warp |= is_gmwarp != 0;
                let warp = &raw mut (*inter_data).warp[i];
                (*warp).is_warp = is_gmwarp;
                if is_gmwarp != 0 {
                    for m in 0..6 {
                        (*warp).mat[m] = wm.wmmat[m];
                    }
                    (*warp).alpha = c_int::from(wm.alpha);
                    (*warp).beta = c_int::from(wm.beta);
                    (*warp).delta = c_int::from(wm.delta);
                    (*warp).gamma = c_int::from(wm.gamma);
                }
            }

            // 2 Run independent blocks: the shaders by list type, those for scaled references when a
            // reference of the frame has another size.
            let scaled = td.scale_enable != 0;
            let wi_per_block: u32 = if scaled { 4 } else { 1 };
            let warp_comp = &raw const (*shaders).inter_warp_comp;
            let (inter_base, inter_comp) = if INTER_LEGACY.load(Ordering::Relaxed) != 0 {
                (&raw const (*shaders).inter_base, &raw const (*shaders).inter_comp)
            } else {
                (&raw const (*shaders).inter_base_v2, &raw const (*shaders).inter_comp_v2)
            };
            let shader_of = |list_type: c_int| -> *const ComputeShader {
                match (list_type, scaled) {
                    (0, _) => &raw const (*shaders).inter_warp,
                    (1, true) => &raw const (*shaders).inter_scale,
                    (2, true) => &raw const (*shaders).inter_scale_comp,
                    (3, true) => &raw const (*shaders).inter_scale_comp_diff_y,
                    (4, true) => &raw const (*shaders).inter_scale_comp_masked,
                    (6, true) => &raw const (*shaders).inter_scale_comp_diff_uv,
                    (8, true) => &raw const (*shaders).inter_scale_obmc_above,
                    (9, true) => &raw const (*shaders).inter_scale_obmc_left,
                    (10, true) => &raw const (*shaders).inter_scale_2x2,
                    (1, false) => inter_base,
                    (2, false) => inter_comp,
                    (3, false) => &raw const (*shaders).inter_comp_diff_y,
                    (4, false) => &raw const (*shaders).inter_comp_masked,
                    (6, false) => &raw const (*shaders).inter_comp_diff_uv,
                    (8, false) => &raw const (*shaders).inter_obmc_above,
                    (9, false) => &raw const (*shaders).inter_obmc_left,
                    (10, false) => &raw const (*shaders).inter_2x2,
                    _ => warp_comp,
                }
            };
            // The lists of a type in the block map.
            let lists_of = |list_type: c_int| block_map.add((list_type as usize - 1) * SIZES);

            commands.set_layout(pipeline(&raw const (*shaders).inter_base).layout());
            commands.bind_read(0, blocks.location());
            commands.bind_read(1, residuals.location());
            commands.bind_read(2, mask_lut.location());
            commands.bind_read(3, buffer((*dec).prediction_blocks_warp).location());
            commands.bind_read(4, buffer((*dec).inter_warp_filter).location());
            commands.bind_write(5, frame_buffers.location());
            commands.bind_constants(6, constants.location);
            // -gputimedetail: the section of the breakdown a type of pass 1 belongs to.
            const SECTION_OF_TYPE: [c_int; CompoundMasked as usize + 1] = [
                AV1_PERF_INTER_WARP,
                AV1_PERF_INTER_SINGLE,
                AV1_PERF_INTER_COMP_AVG,
                AV1_PERF_INTER_COMP_DIFF_Y,
                AV1_PERF_INTER_COMP_MASKED,
            ];
            let inter_fast = td.inter_fast != 0 && !block_map_fast.is_null();
            for t in Warp..=CompoundMasked {
                if inter_fast && (t == CasualInter || t == super::types::CompoundAvrg) {
                    continue;
                }
                let threads = if t == Warp { 1 } else { wi_per_block };
                let (offsets, list) = if t == Warp {
                    (block_map_wrp, gpu_pred_lists::WARP_LIST)
                } else {
                    (lists_of(t), ((t - 1) * InterSizesAllCommon) as u32)
                };
                // A thread of an unscaled compound shader writes 4x2 samples.
                let extra_y = u32::from(!scaled && t > CasualInter);
                pass.lists(
                    shader_of(t),
                    offsets,
                    list,
                    INTER_BLOCK_WIDTH_LUT.len(),
                    SECTION_OF_TYPE[t as usize],
                    |i| ListWork::inter_size(i, extra_y, threads),
                );
            }

            // -interfast: the lists by motion vector class, which have the blocks of the types CasualInter and
            // CompoundAvrg on such a frame (their lists by type are empty). One dispatch per class and block
            // size. The class goes to the fast shaders in the bits 8 and up of width_log2; the lists of class 3
            // (both references of class 3 for a compound block) run the original shaders, which do the same
            // for any class.
            if inter_fast {
                for list in 0..InterFastLists / InterSizesAllCommon {
                    let compound = list >= InterFastClasses;
                    let cls = if compound { list - InterFastClasses } else { list };
                    // The last class has the original shader anyway; so have all classes when the mode asks for
                    // it (av1_set_inter_fast: the lists without the kernels).
                    let original = cls
                        == (if compound { InterFastClasses * InterFastClasses - 1 } else { InterFast2D })
                        || (td.inter_fast
                            & if compound { AV1_INTER_FAST_COMPOUND_ORIGINAL } else { AV1_INTER_FAST_SINGLE_ORIGINAL })
                            != 0;
                    let list_shader = match (compound, original) {
                        (true, true) => &raw const (*shaders).inter_comp,
                        (true, false) => &raw const (*shaders).inter_comp_fast,
                        (false, true) => &raw const (*shaders).inter_base,
                        (false, false) => &raw const (*shaders).inter_fast,
                    };
                    let section =
                        (if compound { AV1_PERF_INTER_COMP_AVG_FAST } else { AV1_PERF_INTER_SINGLE_COPY }) + cls;
                    let first = list as usize * SIZES;
                    pass.lists(
                        list_shader,
                        block_map_fast.add(first),
                        gpu_pred_lists::FAST_LIST + first as u32,
                        INTER_BLOCK_WIDTH_LUT.len(),
                        section,
                        |i| {
                            let mut work = ListWork::inter_size(i, u32::from(compound), 1);
                            if !original {
                                work.width_log2 |= (cls as u32) << 8;
                            }
                            work
                        },
                    );
                }
            }

            // Compound blocks with a warped reference, luma and chroma: a thread per column of a 4x4.
            let warp_compound = |pass: &mut InterPass<'_>, list_type: c_int, section: c_int| {
                pass.lists(
                    warp_comp,
                    lists_of(list_type),
                    ((list_type - 1) * InterSizesAllCommon) as u32,
                    INTER_BLOCK_WIDTH_LUT.len(),
                    section,
                    |i| ListWork::inter_size(i, 0, 4),
                );
            };
            if enable_gm_warp {
                warp_compound(&mut pass, CompoundGlobalWarp, AV1_PERF_INTER_COMP_WARP);
            }

            // The chroma of blocks under 8x8, 2x2 samples at a time.
            pass.lists(
                shader_of(10),
                block_map.add(Inter2x2ArrOffset as usize),
                Inter2x2ArrOffset as u32,
                1,
                AV1_PERF_INTER_CHROMA_2X2,
                |_| ListWork {
                    width_log2: 0,
                    height_log2: 0,
                    threads_per_record: 1 << td.scale_enable,
                    width: 2,
                    height: 2,
                },
            );

            // sync 0
            commands.write_barrier(frame_buffers);
            pass.detail.sync(AV1_PERF_INTER_SYNC0);

            // The chroma of compound blocks with a mask from the difference of the references: it reads
            // the luma prediction of the block.
            pass.lists(
                shader_of(CompoundDiffUv),
                lists_of(CompoundDiffUv),
                ((CompoundDiffUv - 1) * InterSizesAllCommon) as u32,
                INTER_BLOCK_WIDTH_LUT.len(),
                AV1_PERF_INTER_COMP_DIFF_UV,
                |i| ListWork::inter_size(i, u32::from(!scaled), wi_per_block),
            );

            if enable_gm_warp {
                warp_compound(&mut pass, CompoundDiffUvGlobalWarp, AV1_PERF_INTER_COMP_WARP_DIFF_UV);
            }

            // Overlapped prediction from the blocks above, then, behind a barrier, from the blocks to the
            // left. The place of a list is its size: (w << 2) | h above, (h << 2) | w to the left.
            pass.lists(
                shader_of(ObmcAbove),
                lists_of(ObmcAbove),
                ((ObmcAbove - 1) * InterSizesAllCommon) as u32,
                SIZES,
                AV1_PERF_INTER_OBMC_ABOVE,
                |i| ListWork::new((i >> 2) as u32, (i & 3) as u32, 0, wi_per_block),
            );

            // sync 1
            commands.write_barrier(frame_buffers);
            pass.detail.sync(AV1_PERF_INTER_SYNC1);

            // pass 2: Obmc-left
            pass.lists(
                shader_of(ObmcLeft),
                lists_of(ObmcLeft),
                ((ObmcLeft - 1) * InterSizesAllCommon) as u32,
                SIZES,
                AV1_PERF_INTER_OBMC_LEFT,
                |i| ListWork::new((i & 3) as u32, (i >> 2) as u32, 0, wi_per_block),
            );

            // sync 2: the timestamp of the stage is the one behind it.
            commands.write_barrier(frame_buffers);
            perf_gpu_mark(dec, AV1_PERF_GPU_INTER);
            stage_dump_fb(dec, c"inter", td.frame_buffer, std::ptr::null());
        }

        let recon_map = block_map.add(ReconBlockOffset as usize);
        let have_recon = if pass.gpu.is_some() {
            std::slice::from_raw_parts(tiles, tile_count as usize).iter().any(|tile| tile.have_recon != 0)
        } else {
            *recon_map.add(ReconstructBlockSizes as usize) != *recon_map
        };
        if td.intra_iters >= 0 || have_recon {
            let constants = alloc_constants::<IntraData>(&mut td.command_buffer);
            let data = constants.data;
            let intra_main: *const ComputeShader = if INTRA_OPT.load(Ordering::Relaxed) != 0 {
                &raw const (*shaders).intra_main
            } else {
                &raw const (*shaders).intra_main_legacy
            };

            (*data).flags[0] = c_int::from((*cm).seq_params.enable_intra_edge_filter);
            (*data).flags[1] = INTRA_EDGE_FIX.load(Ordering::Relaxed);
            (*data).flags[2] = 0;
            (*data).flags[3] = 0;
            (*data).planes = plane_words(&*td.frame_buffer);
            let taps = &super::libaom::FILTER_INTRA_TAPS;
            for m in 0..5 {
                for k in 0..8 {
                    for i in 0..8 {
                        (*data).filter[m][k][i] = c_int::from(taps[m][k][i]);
                    }
                }
            }
            (&raw mut (*data).mode_params_lut).cast::<[c_int; 16 * 7 * 4]>().write(INTRA_MODE_SHADER_PARAMS);
            for i in 0..128 {
                (*data).sm_weight_arrays[i][0] = c_int::from(SM_WEIGHT_ARRAYS[i]);
            }

            commands.set_layout(pipeline(intra_main).layout());
            commands.bind_read(0, blocks.location());
            commands.bind_read(1, residuals.location());
            commands.bind_write(3, frame_buffers.location());
            commands.bind_constants(4, constants.location);
            if have_recon {
                commands.set_pipeline(pipeline(&raw const (*shaders).reconstruct_block));
                // A slot has a palette buffer once a frame that allows screen content tools has used it.
                // Without one the frame has no palette block, and the shader loads from this buffer for palette
                // blocks only; the binding still needs a buffer, so it gets another upload buffer of the decoder.
                let palette = if !td.palette_buffer.is_null() { td.palette_buffer } else { (*dec).idct_blocks_pool };
                commands.bind_read(2, buffer(palette).location());
                let fb_base_offset = (*td.frame_buffer).base_offset as c_int;
                if let Some(gpu) = &mut pass.gpu {
                    commands.set_inline_constants(5, &[fb_base_offset, 0, 0, 0, 0, 0], 4);
                    gpu.batch(
                        commands,
                        DispatchKind::Recon,
                        0,
                        (0..ReconstructBlockSizes as u32).map(|i| {
                            let (w, h) = (i % 6, i / 6);
                            [ReconBlockOffset as u32 + i, 1 << (w + h), w, h]
                        }),
                    );
                } else {
                    for i in 0..ReconstructBlockSizes as usize {
                        let (offset, count) = list_range(recon_map, i);
                        if count != 0 {
                            let width_log2 = (i % 6) as c_int;
                            let height_log2 = (i / 6) as c_int;
                            let wi_count = count << (width_log2 + height_log2);
                            // wi_count, pass_offset, width_log2, height_log2, fb_base_offset, five unused words
                            let inline = [wi_count, offset, width_log2, height_log2, fb_base_offset, 0, 0, 0, 0, 0];
                            commands.set_inline_constants(5, &inline, 0);
                            commands.dispatch(((wi_count + 63) >> 6) as u32, 1, 1);
                            pass.detail.dispatch(
                                AV1_PERF_GPU_RECON,
                                AV1_PERF_INTER_RECON,
                                i as c_int,
                                4 << width_log2,
                                4 << height_log2,
                                count,
                                wi_count as u32,
                            );
                        }
                    }
                }

                commands.write_barrier(frame_buffers);
                perf_gpu_mark(dec, AV1_PERF_GPU_RECON);
                stage_dump_fb(dec, c"recon", td.frame_buffer, std::ptr::null());
            }

            if td.intra_iters >= 0 && td.intra_v2 != 0 {
                // -intrav2: one thread group per region entry (a 64x64 region in one step), the wavefront inside
                // a region runs in the group. A barrier is only needed between steps: groups of one step never
                // read each other's pixels.
                let sched = &td.intra_v2_sched;
                let entries = td.intra_v2_list_count as usize;
                let list = std::slice::from_raw_parts_mut(sched.list, entries);
                intra_v2::sort(list);
                let v2_shader = pipeline(if td.is_hbd != 0 {
                    &raw const (*lib).shader_intra_v2_hbd
                } else {
                    &raw const (*lib).shader_intra_v2
                });
                commands.set_pipeline(v2_shader);
                commands.set_layout(v2_shader.layout());
                commands.bind_read(0, blocks.location());
                commands.bind_read(1, residuals.location());
                commands.bind_read(2, mask_lut.location());
                commands.bind_read(3, buffer(td.intra_v2_buffer).location());
                commands.bind_read(4, buffer(td.gen_block_map).location());
                commands.bind_write(5, frame_buffers.location());
                commands.bind_constants(6, constants.location);
                // The list lies behind the cell map in the same buffer.
                let list_offset = sched.list.cast::<u8>().offset_from(sched.cells.cast::<u8>()) as u32;
                let mut steps = 0;
                let mut first = 0;
                while let Some(d) = intra_v2::next_dispatch(list, first) {
                    let mut v2_srt =
                        [0, d.step, d.passes, sched.stamp, sched.map_stride as u32, sched.map_uv_offset as u32, 0, 0];
                    // A dispatch takes at most 65535 groups per dimension.
                    let mut done = 0;
                    while done < d.count {
                        let groups = (d.count - done).min(65535);
                        v2_srt[0] = list_offset + 8 * (d.first + done) as u32;
                        if let Some(gpu) = &mut pass.gpu {
                            commands.set_inline_constants(7, &v2_srt[..4], 0);
                            gpu.batch(commands, DispatchKind::IntraV2, groups as u32, [[v2_srt[4], v2_srt[5], 0, 0]]);
                        } else {
                            commands.set_inline_constants(7, &v2_srt, 0);
                            commands.dispatch(groups as u32, 1, 1);
                        }
                        td.intra_stats[0] += 1;
                        done += 65535;
                    }
                    td.intra_stats[2] += d.count;
                    td.intra_stats[3] += d.count * d.passes as c_int;
                    if d.last_of_step != 0 {
                        commands.write_barrier(frame_buffers);
                        td.intra_stats[1] += 1;
                        steps += 1;
                    }
                    first += d.count as usize;
                }
                td.intra_iters = steps - 1; // for the timing record: the last step, like the last iteration
                perf_gpu_mark(dec, AV1_PERF_GPU_INTRA);
                stage_dump_fb(dec, c"intra", td.frame_buffer, std::ptr::null());
            } else if td.intra_iters >= 0 {
                commands.bind_read(2, mask_lut.location());
                // The lists of the wavefront: per iteration nine lists by block size (64x64 down to 4x4 by
                // area), then the filter intra blocks.
                let offsets = block_map.add((InterCountsAll + ReconstructBlockSizes) as usize);
                let intra_filter = &raw const (*shaders).intra_filter;
                let mut shader: *const ComputeShader = std::ptr::null();
                for it in 0..=td.intra_iters {
                    let base = (it * IntraTypeCount) as usize;
                    let offset = *offsets.add(base);
                    // counts0[8], wi_count, pass_offset
                    let mut srt = [0; 10];
                    for size in 0..8 {
                        srt[size] = *offsets.add(base + size + 1) - *offsets.add(base + size);
                    }
                    let counts8 = *offsets.add(base + 9) - *offsets.add(base + 8);
                    // Threads of 4x4 samples: 1024 of them in a 64x64 block down to 8 in a 4x8, 4 in a 4x4.
                    let wi_count = (0..8).map(|size| srt[size] << (10 - size)).sum::<c_int>() + (counts8 << 2);
                    srt[8] = wi_count;
                    srt[9] = offset;
                    if wi_count != 0 {
                        if shader != intra_main {
                            shader = intra_main;
                            commands.set_pipeline(pipeline(shader));
                        }
                        let groups = ((wi_count + 255) >> 8) as u32;
                        if let Some(gpu) = &mut pass.gpu {
                            commands.set_inline_constants(5, &srt[..6], 0);
                            gpu.batch(
                                commands,
                                DispatchKind::Intra,
                                groups,
                                [[srt[6] as u32, srt[7] as u32, srt[8] as u32, srt[9] as u32]],
                            );
                        } else {
                            commands.set_inline_constants(5, &srt, 0);
                            commands.dispatch(groups, 1, 1);
                        }
                        td.intra_stats[0] += 1;
                        td.intra_stats[2] += (wi_count + 255) >> 8;
                    }

                    let (offset, count) = list_range(offsets, base + IntraSizes as usize);
                    if count != 0 {
                        if shader != intra_filter {
                            shader = intra_filter;
                            commands.set_pipeline(pipeline(shader));
                        }
                        if let Some(gpu) = &mut pass.gpu {
                            gpu.batch(
                                commands,
                                DispatchKind::Filter,
                                count as u32,
                                [[(count << 3) as u32, offset as u32, 0, 0]],
                            );
                        } else {
                            commands.set_inline_constants(5, &[count << 3, offset], 0);
                            commands.dispatch(count as u32, 1, 1);
                        }
                        td.intra_stats[0] += 1;
                        td.intra_stats[2] += count;
                    }

                    assert!(count != 0 || wi_count != 0, "a wavefront iteration has blocks");
                    commands.write_barrier(frame_buffers);
                    td.intra_stats[1] += 1;
                }
                td.intra_stats[3] = td.intra_stats[2]; // one pass per thread group
                perf_gpu_mark(dec, AV1_PERF_GPU_INTRA);
                stage_dump_fb(dec, c"intra", td.frame_buffer, std::ptr::null());
            }
        }
        if let Some(gpu) = pass.gpu {
            gpu.finish(commands);
        }
    }
}

// ---------------------------------------------------------------------------------------------------------
// The borders of the frame.
// ---------------------------------------------------------------------------------------------------------

/// The constants of `inter_ext_borders.hlsl`.
#[repr(C)]
struct BordersData {
    planes: [[c_int; 4]; 3],
    dims: [[c_int; 4]; 2],
}

/// `av1_inter_ext_borders` of the C++: records the extension of the finished frame into its borders,
/// where motion vectors of later frames may point.
///
/// # Safety
/// `dec` is the decoder while it records a frame; `cm` is the frame's.
pub(crate) unsafe fn inter_ext_borders(dec: *mut Av1Core, cm: *mut AV1_COMMON) {
    // SAFETY: the caller's contract.
    unsafe {
        let td = &mut *(*dec).curr_frame_data;
        let constants = alloc_constants::<BordersData>(&mut td.command_buffer);
        (*constants.data).planes = plane_words(&*td.dst_frame_buffer);
        let buf = &(*(*cm).cur_frame).buf;
        let (y_crop_width, uv_crop_width) = (buf.crop_widths[0], buf.crop_widths[1]);
        let (y_crop_height, uv_crop_height) = (buf.crop_heights[0], buf.crop_heights[1]);

        (*constants.data).dims[0][0] = y_crop_width;
        (*constants.data).dims[0][1] = y_crop_height;
        (*constants.data).dims[1][0] = uv_crop_width;
        (*constants.data).dims[1][1] = uv_crop_height;

        let shader = pipeline(&raw const (*td.shaders).inter_ext_borders);
        // A thread per row of the three planes.
        let wi_count = y_crop_height + uv_crop_height * 2;
        let commands = commands(dec);
        let frame_buffers = buffer((*dec).frame_buffer_pool);
        commands.set_layout(shader.layout());
        commands.set_pipeline(shader);
        commands.bind_write(0, frame_buffers.location());
        commands.bind_constants(1, constants.location);
        commands.dispatch(((wi_count + 63) / 64) as u32, 1, 1);
        commands.write_barrier(frame_buffers);
        perf_gpu_mark(dec, AV1_PERF_GPU_EXT_BORDERS);
        stage_dump_fb(dec, c"borders", td.dst_frame_buffer, std::ptr::null());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The weights as `av1_prediction_gen_blocks` of the C++ computed them, as it was written.
    #[allow(clippy::needless_late_init)]
    fn dist_wtd_cpp(d0: c_int, d1: c_int) -> c_int {
        let quant_dist_weight = |i: usize, o: usize| QUANT_DIST_WEIGHT[i * 2 + o];
        let quant_dist_lookup_table = |i: usize, o: usize| QUANT_DIST_LOOKUP_TABLE[i * 2 + o];
        let fwd_offset;
        let bck_offset;
        let order = (d0 <= d1) as usize;
        if d0 == 0 || d1 == 0 {
            fwd_offset = quant_dist_lookup_table(3, order);
            bck_offset = quant_dist_lookup_table(3, 1 - order);
        } else {
            let mut i = 0;
            while i < 3 {
                let c0 = quant_dist_weight(i, order);
                let c1 = quant_dist_weight(i, (order == 0) as usize);
                let d0_c0 = d0 * c0;
                let d1_c1 = d1 * c1;
                if (d0 > d1 && d0_c0 < d1_c1) || (d0 <= d1 && d0_c0 > d1_c1) {
                    break;
                }
                i += 1;
            }
            fwd_offset = quant_dist_lookup_table(i, order);
            bck_offset = quant_dist_lookup_table(i, 1 - order);
        }
        fwd_offset | (bck_offset << 4)
    }

    #[test]
    fn distance_weights_as_in_cpp() {
        for d0 in 0..=31 {
            for d1 in 0..=31 {
                assert_eq!(dist_wtd_weights(d0, d1), dist_wtd_cpp(d0, d1), "distances {d0} {d1}");
            }
        }
    }

    #[test]
    fn relative_distance_wraps() {
        let oh = OrderHintInfo {
            enable_order_hint: 1,
            order_hint_bits_minus_1: 6,
            enable_dist_wtd_comp: 0,
            enable_ref_frame_mvs: 0,
        };
        assert_eq!(get_relative_dist(&oh, 5, 3), 2);
        assert_eq!(get_relative_dist(&oh, 3, 5), -2);
        assert_eq!(get_relative_dist(&oh, 1, 127), 2);
        assert_eq!(get_relative_dist(&oh, 127, 1), -2);
        let off = OrderHintInfo { enable_order_hint: 0, ..oh };
        assert_eq!(get_relative_dist(&off, 5, 3), 0);
    }
}
