// SPDX-License-Identifier: BSD-2-Clause
//
// This source code is subject to the terms of the BSD 2 Clause License and the Alliance for Open Media
// Patent License 1.0. If the BSD 2 Clause License was not distributed with this source code in the LICENSE
// file, you can obtain it at www.aomedia.org/license/software. If the Alliance for Open Media Patent
// License 1.0 was not distributed with this source code in the PATENTS file, you can obtain it at
// www.aomedia.org/license/patent.

//! The deblocking loop filter stage: two shaders list the edges of the frame from its mode info, then
//! the vertical edges are filtered in two passes and the horizontal ones in one. From
//! `av1_loopfilter_gpu` of `libav1/dx/loopfilters.cpp`.

use std::ffi::c_int;
use std::sync::atomic::{AtomicI32, Ordering};

use super::core::{
    alloc_constants, buffer, commands, perf_gpu_mark, pipeline, stage_dump_active, stage_dump_constants,
    stage_dump_data, stage_dump_fb,
};
use super::types::{Av1Core, av1_stage_dump_region};
use crate::sys::parser::{AV1_COMMON, AV1_PERF_GPU_LF_GEN, AV1_PERF_GPU_LF_HORZ, AV1_PERF_GPU_LF_VERT, MACROBLOCKD};

unsafe extern "C" {
    /// `av1/common/av1_loopfilter.c`: the filter levels of the frame by segment, reference and mode.
    fn av1_loop_filter_frame_init(cm: *mut AV1_COMMON, plane_start: c_int, plane_end: c_int);
}

/// The constants of the filter shaders.
#[repr(C)]
struct LoopfilterData {
    planes: [[c_int; 4]; 3],
    limits: [[c_int; 4]; 64],
}

/// The constants of the two shaders that list the edges.
#[repr(C)]
struct GenLfData {
    mi_stride: c_int,
    mi_addr_base: c_int,
    delta_q_info_delta_lf_present_flag: c_int,
    delta_q_info_delta_lf_multi: c_int,
    lf_mode_ref_delta_enabled: c_int,
    r: [c_int; 3],

    lf_filter_level: [c_int; 2],
    lf_filter_level_u: c_int,
    lf_filter_level_v: c_int,

    lf_mode_deltas: [[c_int; 4]; 2],
    lf_ref_deltas: [[c_int; 4]; 8],
    seg_features: [[c_int; 4]; 8],
    seg_data: [[[c_int; 4]; 8]; 8],
    lfi_n_lvl: [[[[[c_int; 4]; 2]; 8]; 8]; 3],
}

// Test flag -lfhsplit: how the horizontal loop filter is dispatched. loopfilter_hor works on blocks of 4
// columns by 64 rows; the first edge of a block reads and writes the last rows of the block above it, which
// another thread group of the same dispatch filters. 0: one dispatch per plane, as upstream. 1: the even
// rows of blocks, a barrier, the odd rows. 2: every row of blocks on its own, top to bottom, a barrier
// behind each. Off unless av1_set_lf_h_split() was called.
static LF_H_SPLIT: AtomicI32 = AtomicI32::new(0);

/// `av1_set_lf_h_split` (`dx/av1_perf.h`).
#[unsafe(no_mangle)]
pub extern "C" fn av1_set_lf_h_split(mode: c_int) {
    LF_H_SPLIT.store(mode, Ordering::Relaxed);
}

/// `av1_loopfilter_gpu` of the C++: records the stage for the frame of the current command list.
///
/// # Safety
/// `dec` is the decoder while it records a frame; `cm` and `xd` are those of the frame.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rav1g_loopfilter_gpu(dec: *mut Av1Core, cm: *mut AV1_COMMON, xd: *mut MACROBLOCKD) {
    // SAFETY: the caller's contract: the frame data, the frame buffers, the buffers and pipelines of the
    // decoder and the frame header are live, and only this thread records.
    unsafe {
        if (*cm).lf.filter_level[0] == 0 && (*cm).lf.filter_level[1] == 0 {
            return;
        }
        av1_loop_filter_frame_init(cm, 0, 3);
        let cm = &*cm;
        let lf = &cm.lf;

        let td = &mut *(*dec).curr_frame_data;
        let dst = &*td.dst_frame_buffer;
        let lib = (*dec).shader_lib;
        let commands = commands(dec);
        let lf_blocks = buffer((*dec).loopfilter_blocks);
        let frame_buffers = buffer((*dec).frame_buffer_pool);

        let mi_cols_y = cm.mi_cols.min((dst.y_crop_width + 3) >> 2);
        let mi_rows_y = cm.mi_rows.min((dst.y_crop_height + 3) >> 2);
        let mi_cols_uv = ((cm.mi_cols + 1) >> 1).min((dst.uv_crop_width + 3) >> 2);
        let mi_rows_uv = ((cm.mi_rows + 1) >> 1).min((dst.uv_crop_height + 3) >> 2);
        let mi_cols = |plane: usize| if plane != 0 { mi_cols_uv } else { mi_cols_y };
        let mi_rows = |plane: usize| if plane != 0 { mi_rows_uv } else { mi_rows_y };
        // A plane whose level is 0 is not filtered.
        let filtered =
            |plane: usize| !((plane == 1 && lf.filter_level_u == 0) || (plane == 2 && lf.filter_level_v == 0));

        // Six lists of blocks: vertical edges of Y, U, V (a block is 64 columns by 4 rows), then horizontal
        // edges of Y, U, V (4 columns by 64 rows).
        let blk_cols =
            [(mi_cols_y + 15) >> 4, (mi_cols_uv + 15) >> 4, (mi_cols_uv + 15) >> 4, mi_cols_y, mi_cols_uv, mi_cols_uv];
        let blk_count = [
            blk_cols[0] * mi_rows_y,
            blk_cols[1] * mi_rows_uv,
            blk_cols[2] * mi_rows_uv,
            blk_cols[3] * ((mi_rows_y + 15) >> 4),
            blk_cols[4] * ((mi_rows_uv + 15) >> 4),
            blk_cols[5] * ((mi_rows_uv + 15) >> 4),
        ];
        let mut blk_offsets = [0; 6];
        for i in 1..6 {
            blk_offsets[i] = blk_offsets[i - 1] + blk_count[i - 1];
        }

        let gen_constants = alloc_constants::<GenLfData>(&mut td.command_buffer);
        let gen_data = gen_constants.data;
        (*gen_data).mi_stride = cm.mi_stride;
        // The low half of the address of the mode info pool: the grid holds pointers into it.
        (*gen_data).mi_addr_base = (*(*dec).mode_info_pool).host_ptr as u64 as c_int;
        (*gen_data).delta_q_info_delta_lf_multi = cm.delta_q_info.delta_lf_multi;
        (*gen_data).delta_q_info_delta_lf_present_flag = cm.delta_q_info.delta_lf_present_flag;
        (*gen_data).lf_mode_ref_delta_enabled = c_int::from(lf.mode_ref_delta_enabled);
        (*gen_data).lf_filter_level[0] = lf.filter_level[0];
        (*gen_data).lf_filter_level[1] = lf.filter_level[1];
        (*gen_data).lf_filter_level_u = lf.filter_level_u;
        (*gen_data).lf_filter_level_v = lf.filter_level_v;
        (*gen_data).lf_mode_deltas[0][0] = c_int::from(lf.mode_deltas[0]);
        (*gen_data).lf_mode_deltas[1][0] = c_int::from(lf.mode_deltas[1]);
        for i in 0..8 {
            (*gen_data).lf_ref_deltas[i][0] = c_int::from(lf.ref_deltas[i]);
        }
        for i in 0..8 {
            (*gen_data).seg_features[i][0] = (*xd).lossless[i];
            (*gen_data).seg_features[i][1] = if cm.seg.enabled != 0 { cm.seg.feature_mask[i] as c_int } else { 0 };
            for j in 0..8 {
                (*gen_data).seg_data[i][j][0] = c_int::from(cm.seg.feature_data[i][j]);
            }
        }
        for p in 0..3 {
            for s in 0..8 {
                for r in 0..8 {
                    for m in 0..2 {
                        (*gen_data).lfi_n_lvl[p][s][r][m][0] = c_int::from(cm.lf_info.lvl[p][s][0][r][m]);
                        (*gen_data).lfi_n_lvl[p][s][r][m][1] = c_int::from(cm.lf_info.lvl[p][s][1][r][m]);
                    }
                }
            }
        }

        let shader = pipeline(&raw const (*lib).shader_gen_lf_vert);
        commands.set_layout(shader.layout());
        commands.bind_read(0, buffer((*dec).mode_info_pool).location());
        commands.bind_read(1, buffer(td.mode_info_grid).location());
        commands.bind_write(2, lf_blocks.location());
        commands.bind_constants(3, gen_constants.location);

        commands.set_pipeline(shader);
        for plane in (0..3).filter(|&plane| filtered(plane)) {
            let (mi_cols, mi_rows) = (mi_cols(plane), mi_rows(plane));
            let wicount = mi_rows * ((mi_cols + 15) >> 4);
            // wicount, mi_cols, mi_rows, plane, dst_offset, dst_stride
            let inline = [wicount, mi_cols, mi_rows, plane as c_int, blk_offsets[plane] * 32, blk_cols[plane] * 32];
            commands.set_inline_constants(4, &inline, 0);
            commands.dispatch(((wicount + 63) >> 6) as u32, 1, 1);
        }
        commands.set_pipeline(pipeline(&raw const (*lib).shader_gen_lf_hor));
        for plane in (0..3).filter(|&plane| filtered(plane)) {
            let (mi_cols, mi_rows) = (mi_cols(plane), mi_rows(plane));
            let wicount = mi_cols * ((mi_rows + 15) >> 4);
            let inline =
                [wicount, mi_cols, mi_rows, plane as c_int, blk_offsets[plane + 3] * 32, blk_cols[plane + 3] * 2];
            commands.set_inline_constants(4, &inline, 0);
            commands.dispatch(((wicount + 63) >> 6) as u32, 1, 1);
        }
        commands.write_barrier(lf_blocks);
        perf_gpu_mark(dec, AV1_PERF_GPU_LF_GEN);

        let constants = alloc_constants::<LoopfilterData>(&mut td.command_buffer);
        let data = constants.data;
        let frame_buffer = &*td.frame_buffer;
        for (plane, info) in frame_buffer.planes.iter().enumerate() {
            (*data).planes[plane] = [info.stride, info.offset, info.res_stride, info.res_offset];
        }
        for (level, thresholds) in cm.lf_info.lfthr.iter().enumerate() {
            (*data).limits[level][0] = c_int::from(thresholds.lim[0]);
            (*data).limits[level][1] = c_int::from(thresholds.mblim[0]);
            (*data).limits[level][2] = c_int::from(thresholds.hev_thr[0]);
        }

        // -dumpstages: what the loop filter reads besides the frame, before its first pass. The constants
        // (those of the list shaders, then those of the filters) and the edge lists as gen_lf_blocks_vert/hor
        // have just written them: six lists, vertical edges of Y, U, V and horizontal edges of Y, U, V, each
        // from its first block; a record of 32 bytes per block, sixteen edges of 16 bits. A list of a plane
        // that is not filtered is not written for this frame (on=0) and holds what an earlier frame left
        // there.
        if stage_dump_active(dec) {
            let params = [
                stage_dump_constants(dec, &gen_constants, size_of::<GenLfData>()),
                stage_dump_constants(dec, &constants, size_of::<LoopfilterData>()),
            ];
            let layout = format!(
                "buffer=lf_params gen_size={} lf_size={} bit_depth={}\n",
                size_of::<GenLfData>(),
                size_of::<LoopfilterData>(),
                td.bitdepth
            );
            stage_dump_data(dec, c"lf_params", &layout, 1024, &params);

            let mut layout = String::from("buffer=lf_blocks record=32 lists=6\n");
            for i in 0..6 {
                let plane = i % 3;
                layout.push_str(&format!(
                    "list dir={} plane={} first={} cols={} count={} mi_cols={} mi_rows={} on={}\n",
                    if i < 3 { "vert" } else { "horz" },
                    plane,
                    blk_offsets[i],
                    blk_cols[i],
                    blk_count[i],
                    mi_cols(plane),
                    mi_rows(plane),
                    c_int::from(filtered(plane))
                ));
            }
            let blocks = &*(*dec).loopfilter_blocks;
            let lists = av1_stage_dump_region {
                src: blocks.dev,
                offset: 0,
                size: (((blk_offsets[5] + blk_count[5]) as usize) * 32).min(blocks.size),
                uav: 1,
            };
            stage_dump_data(dec, c"lf_blocks", &layout, 1024, &[lists]);
        }

        // The inline constants of the filters: wicount, plane, offset_base, block_cols, block_id_offset.
        let shader = pipeline(&raw const (*td.shaders).loopfilter_v);
        commands.set_layout(shader.layout());
        commands.bind_read(0, lf_blocks.location());
        commands.bind_write(1, frame_buffers.location());
        commands.bind_constants(2, constants.location);
        commands.set_pipeline(shader);
        // Vertical edges: the even blocks, then the odd ones, so that no two groups of a dispatch touch
        // the same samples.
        for p in (0..3).filter(|&p| filtered(p)) {
            let wicount = ((blk_count[p] + 1) >> 1) * 4;
            commands.set_inline_constants(3, &[wicount, p as c_int, blk_offsets[p], blk_cols[p], 0], 0);
            commands.dispatch(((wicount + 63) >> 6) as u32, 1, 1);
        }
        commands.write_barrier(frame_buffers);
        stage_dump_fb(dec, c"lf_vert_even", td.frame_buffer, std::ptr::null());

        for p in (0..3).filter(|&p| filtered(p)) {
            let wicount = (blk_count[p] >> 1) * 4;
            commands.set_inline_constants(3, &[wicount, p as c_int, blk_offsets[p], blk_cols[p], 1], 0);
            commands.dispatch(((wicount + 63) >> 6) as u32, 1, 1);
        }
        commands.write_barrier(frame_buffers);
        perf_gpu_mark(dec, AV1_PERF_GPU_LF_VERT);
        stage_dump_fb(dec, c"lf_vert", td.frame_buffer, std::ptr::null());

        commands.set_pipeline(pipeline(&raw const (*td.shaders).loopfilter_h));
        let lf_h_split = LF_H_SPLIT.load(Ordering::Relaxed);
        if lf_h_split == 0 {
            for p in (0..3).filter(|&p| filtered(p)) {
                let list = p + 3;
                let wicount = blk_count[list] * 4;
                commands.set_inline_constants(3, &[wicount, p as c_int, blk_offsets[list], blk_cols[list], 0], 0);
                commands.dispatch(((wicount + 63) >> 6) as u32, 1, 1);
            }
            commands.write_barrier(frame_buffers);
        } else {
            // -lfhsplit: one dispatch per row of blocks (64 rows of the plane). Mode 1 takes the even rows of
            // all planes, then the odd ones; mode 2 takes the rows in order. The planes do not touch each
            // other.
            let block_rows = (0..3).map(|p| blk_count[p + 3] / blk_cols[p + 3].max(1)).fold(0, c_int::max);
            let passes = if lf_h_split == 1 { 2 } else { block_rows };
            // Mode 1: every second row from the pass's. Mode 2: the one row of the pass.
            let step = if lf_h_split == 1 { 2 } else { block_rows };
            for pass in 0..passes {
                for p in (0..3).filter(|&p| filtered(p)) {
                    let list = p + 3;
                    let rows = if blk_cols[list] != 0 { blk_count[list] / blk_cols[list] } else { 0 };
                    let wicount = blk_cols[list] * 4;
                    let mut row = pass;
                    while row < rows {
                        let inline = [wicount, p as c_int, blk_offsets[list], blk_cols[list], row * blk_cols[list]];
                        commands.set_inline_constants(3, &inline, 0);
                        commands.dispatch(((wicount + 63) >> 6) as u32, 1, 1);
                        row += step;
                    }
                }
                commands.write_barrier(frame_buffers);
            }
        }
        perf_gpu_mark(dec, AV1_PERF_GPU_LF_HORZ);
        stage_dump_fb(dec, c"lf_horz", td.frame_buffer, std::ptr::null());
    }
}
