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
use super::intra_v2;
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
use crate::sys::parser::{
    ALTREF_FRAME, AV1_COMMON, AV1_INTER_FAST_COMPOUND_ORIGINAL, AV1_INTER_FAST_SINGLE_ORIGINAL,
    AV1_PERF_GPU_EXT_BORDERS, AV1_PERF_GPU_GEN_BLOCKS, AV1_PERF_GPU_INTER, AV1_PERF_GPU_INTRA, AV1_PERF_GPU_RECON,
    AV1_PERF_INTER_CHROMA_2X2, AV1_PERF_INTER_COMP_AVG, AV1_PERF_INTER_COMP_AVG_FAST, AV1_PERF_INTER_COMP_DIFF_UV,
    AV1_PERF_INTER_COMP_DIFF_Y, AV1_PERF_INTER_COMP_MASKED, AV1_PERF_INTER_COMP_WARP, AV1_PERF_INTER_COMP_WARP_DIFF_UV,
    AV1_PERF_INTER_LISTS, AV1_PERF_INTER_OBMC_ABOVE, AV1_PERF_INTER_OBMC_LEFT, AV1_PERF_INTER_RECON,
    AV1_PERF_INTER_SECTION_COUNT, AV1_PERF_INTER_SINGLE, AV1_PERF_INTER_SINGLE_COPY, AV1_PERF_INTER_SYNC0,
    AV1_PERF_INTER_SYNC1, AV1_PERF_INTER_WARP, AV1Decoder, EXTREF_FRAME, HwFrameBuffer, INVALID_IDX, LAST_FRAME,
    MAX_FRAME_DISTANCE, OrderHintInfo, RefCntBuffer, SCALE_SUBPEL_BITS, TileInfo, WarpedMotionParams,
    av1_filter_intra_taps, av1_perf_inter_blocks, av1_perf_inter_detail, av1_perf_inter_work, av1_tile_data,
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
    reserved: [c_int; 3],
    wedge_offsets: [[c_int; 4]; 22],
    dist_wtd: [[c_int; 4]; 8 * 8],
    lossless_seg: [[c_int; 4]; 8],

    global_warp: [[c_int; 4]; 8],
    wm_params: [WarpParams; 8],
}

/// `av1_prediction_gen_blocks` of the C++: turns the counts of the tiles' lists into where each list
/// starts in the block records of the frame, and records the shader that writes the records.
///
/// # Safety
/// `dec` is the decoder while it records a frame whose tiles are decoded; `pbi` is its entropy decoder.
pub(crate) unsafe fn prediction_gen_blocks(pbi: *mut AV1Decoder, dec: *mut Av1Core) {
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
        // The low half of the address of the mode info pool: the grid holds pointers into it.
        (*data).mi_addr_base = (*(*dec).mode_info_pool).host_ptr as u64 as u32;
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
            if let Some(sec) = td.sec_thread_data.as_ref() {
                std::slice::from_raw_parts_mut(sec.tile_data, tile_count).swap(0, main_tile);
            }
        }

        td.intra_iters = intra_iters;
        let pred_block_types = InterCountsAll + ReconstructBlockSizes + (intra_iters + 1) * IntraTypeCount;
        let mut offset = 0;
        for it in 0..=pred_block_types {
            for tile in tiles.iter() {
                if it < tile.gen_pred_map_max {
                    let list = tile.gen_block_map.offset(it as isize);
                    let cnt = *list;
                    *list = offset;
                    offset += cnt;
                }
            }
        }
        let pred_block_count = offset; // records of prediction_blocks the frame's lists take
        offset = 0;
        for it in 0..=SIZES {
            for tile in tiles.iter() {
                let list = tile.gen_block_map_wrp.add(it);
                let cnt = *list;
                *list = offset;
                offset += cnt;
            }
        }
        let pred_warp_count = offset; // records of prediction_blocks_warp
        // -interfast: the lists by motion vector class, behind the lists by type in prediction_blocks. The
        // blocks in them are the ones the lists CasualInter and CompoundAvrg have without the flag, so the
        // buffer holds as many records as it always did.
        let inter_fast = td.inter_fast != 0 && !tiles[0].gen_block_map_fast.is_null();
        let mut pred_block_total = pred_block_count;
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
        // -dumpstages: the mode info, where the first stage that reads it starts.
        stage_dump_mode_info(dec, cm);
        let commands = commands(dec);
        let lib = (*dec).shader_lib;
        let blocks = buffer((*dec).prediction_blocks);
        let warp_blocks = buffer((*dec).prediction_blocks_warp);
        // -interfast: the same shader with the lists by class and one more inline constant, where they are.
        let shader = pipeline(if inter_fast {
            &raw const (*lib).shader_gen_pred_blocks_fast
        } else {
            &raw const (*lib).shader_gen_pred_blocks
        });
        let srt_words = if inter_fast { 8 } else { 7 };
        commands.set_layout(shader.layout());
        commands.set_pipeline(shader);
        commands.bind_read(0, buffer((*dec).mode_info_pool).location());
        commands.bind_read(1, buffer(td.gen_mi_block_indexes).location());
        commands.bind_read(2, buffer(td.gen_block_map).location());
        commands.bind_read(3, buffer(td.mode_info_grid).location());
        commands.bind_read(4, buffer(td.gen_intra_inter_grid).location());
        commands.bind_write(5, blocks.location());
        commands.bind_write(6, warp_blocks.location());
        commands.bind_constants(7, constants.location);
        // The inline constants of a tile: wi_count, mi_offset, mi_idx_base, col_start, row_start,
        // index_offset, index_offset_warp, and for the lists by class index_offset_fast.
        let dispatch_tile = |mi_count: c_int, mi_offset: c_int, tile: &av1_tile_data| {
            let inline = [
                mi_count,
                mi_offset,
                tile.gen_index_base,
                tile.mi_col_start,
                tile.mi_row_start,
                tile.gen_block_map_offset,
                tile.gen_block_warp_offset,
                tile.gen_block_fast_offset,
            ];
            commands.set_inline_constants(8, &inline[..srt_words], 0);
            commands.dispatch(((mi_count + 63) >> 6) as u32, 1, 1);
        };
        for tile in tiles.iter() {
            dispatch_tile(tile.mi_count, tile.mi_offset, tile);
        }
        // The mode info of a frame that took a second slot's share of the pool: the same tiles, the part
        // of their mode info that lies there.
        if let Some(sec) = td.sec_thread_data.as_ref() {
            let tiles2 = std::slice::from_raw_parts(sec.tile_data, tile_count);
            for (tile, tile2) in tiles.iter().zip(tiles2) {
                if tile2.mi_count != 0 {
                    dispatch_tile(tile2.mi_count, tile2.mi_offset, tile);
                }
            }
        }
        commands.write_barrier2(blocks, warp_blocks);
        perf_gpu_mark(dec, AV1_PERF_GPU_GEN_BLOCKS);
        // -dumpstages: the block records gen_pred_blocks has just written, as the prediction stages read
        // them.
        if stage_dump_active(dec) {
            const PRED_BLOCK_SIZE: usize = 16;
            const PRED_WARP_BLOCK_SIZE: usize = 48;
            let object = &*(*dec).prediction_blocks;
            let size = (pred_block_total as usize * PRED_BLOCK_SIZE).min(object.size);
            let region = av1_stage_dump_region { src: object.dev, offset: 0, size, uav: 1 };
            let layout = format!("buffer=pred_blocks record={PRED_BLOCK_SIZE} count={}\n", size / PRED_BLOCK_SIZE);
            stage_dump_data(dec, c"pred_blocks", &layout, 160, &[region]);
            let object = &*(*dec).prediction_blocks_warp;
            let size = (pred_warp_count as usize * PRED_WARP_BLOCK_SIZE).min(object.size);
            let region = av1_stage_dump_region { src: object.dev, offset: 0, size, uav: 1 };
            let layout = format!(
                "buffer=pred_blocks_warp record={PRED_WARP_BLOCK_SIZE} count={}\n",
                size / PRED_WARP_BLOCK_SIZE
            );
            stage_dump_data(dec, c"pred_blocks_warp", &layout, 160, &[region]);
        }
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

// Test player switch (-intraopt): the original intra path takes intra_main from the pipeline state built
// with FXC optimisation (intra_main_opt.hlsl / intra_main_hbd_opt.hlsl, which only include the original
// source) instead of the /Od build the project asks for. Off unless av1_set_intra_opt(1) was called.
static INTRA_OPT: AtomicI32 = AtomicI32::new(0);

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
    /// The loads of the reference rows, from the shaders (docs/gpu-av1-decoder.md, "Inter prediction
    /// breakdown"): rows per thread and reference times the bytes of the Load4/Load3/Load2/Load of one row.
    /// They do not depend on the motion vector or the filter: every shader reads the rows of an 8-tap
    /// filter in both directions. A shader for scaled references reads 8 rows plus what its 4 output rows
    /// step over, which is 11 for a reference of the frame's size and more for a larger one: the estimate
    /// takes the largest step of the frame's references.
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
}

impl InterPass<'_> {
    /// One list of blocks: the pipeline if it is another than the last, the inline constants (threads
    /// with work, the first record of the list, log2 of the block's width and height in 4x4 units, with
    /// whatever a shader has above them), the dispatch.
    ///
    /// # Safety
    /// `shader` is a pipeline of the decoder that records.
    #[inline]
    unsafe fn run(
        &mut self,
        shader: *const ComputeShader,
        pass_offset: c_int,
        width_log2: u32,
        height_log2: u32,
        wi_count: u32,
    ) {
        if self.shader != shader {
            self.shader = shader;
            // SAFETY: the caller's contract.
            self.commands.set_pipeline(unsafe { pipeline(shader) });
        }
        self.commands.set_inline_constants(7, &[wi_count, pass_offset as u32, width_log2, height_log2], 0);
        self.commands.dispatch((wi_count + 63) >> 6, 1, 1);
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
pub(crate) unsafe fn prediction_run_all(dec: *mut Av1Core, cm: *mut AV1_COMMON, _tile: *mut TileInfo) {
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
            d: perf_inter_begin(dec),
            commands,
            frame_buffers,
            thread_bytes: [0; AV1_PERF_INTER_SECTION_COUNT as usize],
        };
        detail.init(td);
        if let Some(d) = detail.d.as_mut() {
            d.has_inter = c_int::from(do_inter);
            let counters = &(*dec).perf_inter_tiles;
            detail.add_tiles(&counters[..counters.len().min(usize::try_from(tile_count).unwrap_or(0))]);
        }
        let mut pass = InterPass { commands, shader: std::ptr::null(), detail };
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
                    (1, false) => &raw const (*shaders).inter_base,
                    (2, false) => &raw const (*shaders).inter_comp,
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
            for t in Warp..=CompoundMasked {
                let wi_per_subb = if t == Warp { 1 } else { wi_per_block };
                let offsets = if t == Warp { block_map_wrp } else { lists_of(t) };
                // A thread of a compound shader for references of the frame's size: 4x2 samples.
                let is_4x2 = u32::from(!scaled && t > CasualInter);
                let section = SECTION_OF_TYPE[t as usize];
                for i in 0..SIZES {
                    let (offset, count) = list_range(offsets, i);
                    if count == 0 {
                        continue;
                    }
                    let width_log2 = INTER_BLOCK_WIDTH_LUT[i] as u32;
                    let height_log2 = INTER_BLOCK_HEIGHT_LUT[i] as u32 + is_4x2;
                    let wi_count = (wi_per_subb << (width_log2 + height_log2)) * count as u32;
                    pass.run(shader_of(t), offset, width_log2, height_log2, wi_count);
                    pass.detail.dispatch(
                        AV1_PERF_GPU_INTER,
                        section,
                        i as c_int,
                        4 << INTER_BLOCK_WIDTH_LUT[i],
                        4 << INTER_BLOCK_HEIGHT_LUT[i],
                        count,
                        wi_count,
                    );
                }
                pass.detail.section(section);
            }

            // -interfast: the lists by motion vector class, which have the blocks of the types CasualInter and
            // CompoundAvrg on such a frame (their lists by type are empty). One dispatch per class and block
            // size. The class goes to the fast shaders in the bits 8 and up of width_log2; the lists of class 3
            // (both references of class 3 for a compound block) run the original shaders, which do the same
            // for any class.
            if td.inter_fast != 0 && !block_map_fast.is_null() {
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
                    let offsets = block_map_fast.add(list as usize * SIZES);
                    for i in 0..SIZES {
                        let (offset, count) = list_range(offsets, i);
                        if count == 0 {
                            continue;
                        }
                        let mut width_log2 = INTER_BLOCK_WIDTH_LUT[i] as u32;
                        // A thread of a compound shader: 4x2 samples.
                        let height_log2 = INTER_BLOCK_HEIGHT_LUT[i] as u32 + u32::from(compound);
                        let wi_count = (1 << (width_log2 + height_log2)) * count as u32;
                        if !original {
                            width_log2 |= (cls as u32) << 8;
                        }
                        pass.run(list_shader, offset, width_log2, height_log2, wi_count);
                        pass.detail.dispatch(
                            AV1_PERF_GPU_INTER,
                            section,
                            i as c_int,
                            4 << INTER_BLOCK_WIDTH_LUT[i],
                            4 << INTER_BLOCK_HEIGHT_LUT[i],
                            count,
                            wi_count,
                        );
                    }
                    pass.detail.section(section);
                }
            }

            // Compound blocks with a warped reference, luma and chroma: a thread per column of a 4x4.
            let warp_compound = |pass: &mut InterPass<'_>, list_type: c_int, section: c_int| {
                let offsets = lists_of(list_type);
                for i in 0..SIZES {
                    let (offset, count) = list_range(offsets, i);
                    if count == 0 {
                        continue;
                    }
                    let width_log2 = INTER_BLOCK_WIDTH_LUT[i] as u32;
                    let height_log2 = INTER_BLOCK_HEIGHT_LUT[i] as u32;
                    let wi_count = (4 << (width_log2 + height_log2)) * count as u32;
                    pass.run(warp_comp, offset, width_log2, height_log2, wi_count);
                    pass.detail.dispatch(
                        AV1_PERF_GPU_INTER,
                        section,
                        i as c_int,
                        4 << width_log2,
                        4 << height_log2,
                        count,
                        wi_count,
                    );
                }
                pass.detail.section(section);
            };
            if enable_gm_warp {
                warp_compound(&mut pass, CompoundGlobalWarp, AV1_PERF_INTER_COMP_WARP);
            }

            // The chroma of blocks under 8x8, 2x2 samples at a time.
            let (offset, count) = list_range(block_map, Inter2x2ArrOffset as usize);
            if count > 0 {
                let wi_count = (count << td.scale_enable) as u32;
                pass.shader = shader_of(10);
                commands.set_pipeline(pipeline(pass.shader));
                commands.set_inline_constants(7, &[wi_count, offset as u32, 0, 0], 0);
                commands.dispatch((wi_count + 63) >> 6, 1, 1);
                pass.detail.dispatch(AV1_PERF_GPU_INTER, AV1_PERF_INTER_CHROMA_2X2, 0, 2, 2, count, wi_count);
                pass.detail.section(AV1_PERF_INTER_CHROMA_2X2);
            }

            // sync 0
            commands.write_barrier(frame_buffers);
            pass.detail.sync(AV1_PERF_INTER_SYNC0);

            // The chroma of compound blocks with a mask from the difference of the references: it reads
            // the luma prediction of the block.
            let offsets = lists_of(CompoundDiffUv);
            for i in 0..SIZES {
                let (offset, count) = list_range(offsets, i);
                if count == 0 {
                    continue;
                }
                let width_log2 = INTER_BLOCK_WIDTH_LUT[i] as u32;
                let height_log2 = INTER_BLOCK_HEIGHT_LUT[i] as u32 + u32::from(!scaled);
                let wi_count = (wi_per_block << (width_log2 + height_log2)) * count as u32;
                pass.run(shader_of(CompoundDiffUv), offset, width_log2, height_log2, wi_count);
                pass.detail.dispatch(
                    AV1_PERF_GPU_INTER,
                    AV1_PERF_INTER_COMP_DIFF_UV,
                    i as c_int,
                    4 << INTER_BLOCK_WIDTH_LUT[i],
                    4 << INTER_BLOCK_HEIGHT_LUT[i],
                    count,
                    wi_count,
                );
            }
            pass.detail.section(AV1_PERF_INTER_COMP_DIFF_UV);

            if enable_gm_warp {
                warp_compound(&mut pass, CompoundDiffUvGlobalWarp, AV1_PERF_INTER_COMP_WARP_DIFF_UV);
            }

            // Overlapped prediction from the blocks above, then, behind a barrier, from the blocks to the
            // left. The place of a list is its size: (w << 2) | h above, (h << 2) | w to the left.
            let offsets = lists_of(ObmcAbove);
            for i in 0..SIZES {
                let (offset, count) = list_range(offsets, i);
                if count == 0 {
                    continue;
                }
                let width_log2 = (i >> 2) as u32;
                let height_log2 = (i & 3) as u32;
                let wi_count = (wi_per_block << (width_log2 + height_log2)) * count as u32;
                pass.run(shader_of(ObmcAbove), offset, width_log2, height_log2, wi_count);
                pass.detail.dispatch(
                    AV1_PERF_GPU_INTER,
                    AV1_PERF_INTER_OBMC_ABOVE,
                    i as c_int,
                    4 << width_log2,
                    4 << height_log2,
                    count,
                    wi_count,
                );
            }
            pass.detail.section(AV1_PERF_INTER_OBMC_ABOVE);

            // sync 1
            commands.write_barrier(frame_buffers);
            pass.detail.sync(AV1_PERF_INTER_SYNC1);

            // pass 2: Obmc-left
            let offsets = lists_of(ObmcLeft);
            for i in 0..SIZES {
                let (offset, count) = list_range(offsets, i);
                if count == 0 {
                    continue;
                }
                let width_log2 = (i & 3) as u32;
                let height_log2 = (i >> 2) as u32;
                let wi_count = (wi_per_block << (width_log2 + height_log2)) * count as u32;
                pass.run(shader_of(ObmcLeft), offset, width_log2, height_log2, wi_count);
                pass.detail.dispatch(
                    AV1_PERF_GPU_INTER,
                    AV1_PERF_INTER_OBMC_LEFT,
                    i as c_int,
                    4 << width_log2,
                    4 << height_log2,
                    count,
                    wi_count,
                );
            }
            pass.detail.section(AV1_PERF_INTER_OBMC_LEFT);

            // sync 2: the timestamp of the stage is the one behind it.
            commands.write_barrier(frame_buffers);
            perf_gpu_mark(dec, AV1_PERF_GPU_INTER);
            stage_dump_fb(dec, c"inter", td.frame_buffer, std::ptr::null());
        }

        let recon_map = block_map.add(ReconBlockOffset as usize);
        let recon_blocks = *recon_map.add(ReconstructBlockSizes as usize) - *recon_map;
        if td.intra_iters >= 0 || recon_blocks != 0 {
            let constants = alloc_constants::<IntraData>(&mut td.command_buffer);
            let data = constants.data;
            // The /Od build unless -intraopt asks for the optimised build of the same source (same layout).
            let intra_main: *const ComputeShader = if INTRA_OPT.load(Ordering::Relaxed) != 0 {
                &raw const (*shaders).intra_main_opt
            } else {
                &raw const (*shaders).intra_main
            };

            (*data).flags[0] = c_int::from((*cm).seq_params.enable_intra_edge_filter);
            (*data).flags[1] = INTRA_EDGE_FIX.load(Ordering::Relaxed);
            (*data).flags[2] = 0;
            (*data).flags[3] = 0;
            (*data).planes = plane_words(&*td.frame_buffer);
            let taps = (&raw const av1_filter_intra_taps).read();
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
            if recon_blocks != 0 {
                commands.set_pipeline(pipeline(&raw const (*shaders).reconstruct_block));
                // A slot has a palette buffer once a frame that allows screen content tools has used it.
                // Without one the frame has no palette block, and the shader loads from this buffer for palette
                // blocks only; the binding still needs a buffer, so it gets another upload buffer of the slot.
                let palette = if td.palette_buffer.is_null() { td.idct_blocks_unordered } else { td.palette_buffer };
                commands.bind_read(2, buffer(palette).location());
                let fb_base_offset = (*td.frame_buffer).base_offset as c_int;
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
                        commands.set_inline_constants(7, &v2_srt, 0);
                        commands.dispatch(groups as u32, 1, 1);
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
                        commands.set_inline_constants(5, &srt, 0);
                        commands.dispatch(((wi_count + 255) >> 8) as u32, 1, 1);
                        td.intra_stats[0] += 1;
                        td.intra_stats[2] += (wi_count + 255) >> 8;
                    }

                    let (offset, count) = list_range(offsets, base + IntraSizes as usize);
                    if count != 0 {
                        if shader != intra_filter {
                            shader = intra_filter;
                            commands.set_pipeline(pipeline(shader));
                        }
                        commands.set_inline_constants(5, &[count << 3, offset], 0);
                        commands.dispatch(count as u32, 1, 1);
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
        let (y_crop_width, uv_crop_width) = (buf.__bindgen_anon_3.crop_widths[0], buf.__bindgen_anon_3.crop_widths[1]);
        let (y_crop_height, uv_crop_height) =
            (buf.__bindgen_anon_4.crop_heights[0], buf.__bindgen_anon_4.crop_heights[1]);

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
