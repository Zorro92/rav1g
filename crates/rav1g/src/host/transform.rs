// SPDX-License-Identifier: BSD-2-Clause
//
// This source code is subject to the terms of the BSD 2 Clause License and the Alliance for Open Media
// Patent License 1.0. If the BSD 2 Clause License was not distributed with this source code in the LICENSE
// file, you can obtain it at www.aomedia.org/license/software. If the Alliance for Open Media Patent
// License 1.0 was not distributed with this source code in the PATENTS file, you can obtain it at
// www.aomedia.org/license/patent.

//! The inverse transform stage: the residual buffer is cleared, the transform blocks of the tiles are
//! sorted by transform size, and one dispatch per size transforms them. From `libav1/dx/transform.cpp`.

use std::ffi::c_int;
use std::sync::atomic::{AtomicI32, Ordering};

use super::core::{
    alloc_constant_bytes, alloc_constants, buffer, commands, layout, perf_gpu_mark, pipeline, stage_dump_residuals,
};
use super::frame::joined_runs;
use super::tables::{TX_SIZE_HIGH, TX_SIZE_WIDE};
use super::types::Av1Core;
use crate::sys::{
    AV1_PERF_GPU_IDCT, AV1_PERF_GPU_IDCT_SORT, AV1_PERF_GPU_RESIDUAL_CLEAR, TX_4X4, TX_4X8, TX_8X4, TX_8X8,
    TX_SIZES_ALL,
};

// av1_set_residual_clear_legacy(1) restores the per-frame full-buffer clear before idct (-residualclearlegacy).
static RESIDUAL_CLEAR_LEGACY: AtomicI32 = AtomicI32::new(0);
/// `av1_set_residual_clear_legacy` (`dx/av1_perf.h`).
#[unsafe(no_mangle)]
pub extern "C" fn av1_set_residual_clear_legacy(enable: c_int) {
    RESIDUAL_CLEAR_LEGACY.store(enable, Ordering::Relaxed);
}

fn residual_clear_legacy() -> bool {
    RESIDUAL_CLEAR_LEGACY.load(Ordering::Relaxed) != 0
}

/// Transform types of the sort: the transform sizes and the lossless transform.
const TX_TYPES: usize = TX_SIZES_ALL as usize + 1;

/// The constants of `idct_sort_blocks.hlsl` for one tile.
#[repr(C)]
struct SortData {
    count: c_int,
    src_offset: c_int,
    reserved: [c_int; 2],
    offsets: [[c_int; 4]; 20],
}

/// The constants of the transform shaders for the frame.
#[repr(C)]
struct IdctFrameData {
    planes: [[c_int; 4]; 3],
    bitdepth: c_int,
}

const _: () = assert!(TX_TYPES == 20);

/// `av1_idct_run` of the C++: records the stage for the frame of the current command list.
///
/// # Safety
/// `dec` is the decoder while it records a frame whose tiles are decoded.
pub(crate) unsafe fn idct_run(dec: *mut Av1Core) {
    // SAFETY: the caller's contract: the frame data, its tiles, its frame buffer and the buffers and
    // pipelines of the decoder are live, and only this thread records.
    unsafe {
        let td = &mut *(*dec).curr_frame_data;
        let tiles = std::slice::from_raw_parts_mut(td.tile_data, td.tile_count as usize);

        // The counts of the tiles per transform type become where their blocks start in the sorted list:
        // type by type, in each type tile by tile.
        let mut offset = 0;
        for tx_type in 0..=TX_TYPES {
            for tile in tiles.iter_mut() {
                let count = tile.idct_blocks_sizes[tx_type];
                tile.idct_blocks_sizes[tx_type] = offset;
                offset += count;
            }
        }

        let lib = (*dec).shader_lib;
        let commands = commands(dec);
        let residuals = buffer((*dec).idct_residuals);
        let blocks = buffer((*dec).idct_blocks);
        let frame_buffer = &*td.frame_buffer;

        let legacy_clear = residual_clear_legacy();
        let mut need_residual_barrier = false;
        if legacy_clear {
            (*dec).residuals_initialized = 0;
            let shader = pipeline(&raw const (*lib).shader_fill_buffer);
            commands.set_layout(shader.layout());
            commands.set_pipeline(shader);
            commands.bind_write(0, residuals.location());
            let words = super::residual::clear_work_items(
                (*(*dec).idct_residuals).size,
                frame_buffer.planes[1].res_offset,
                frame_buffer.planes[2].res_offset,
            );
            let value = [words, 0];
            commands.set_inline_constants(1, &value, 0);
            commands.dispatch(((value[0] + 63) / 64) as u32, 1, 1);
            perf_gpu_mark(dec, AV1_PERF_GPU_RESIDUAL_CLEAR);
            need_residual_barrier = true;
        } else if (*dec).residuals_initialized == 0 {
            // Initialize the entire residual buffer once on the first submitted frame. Every frame
            // thereafter zeroes only the transform blocks it wrote after prediction, maintaining the
            // all-zero invariant across arbitrary resolution changes.
            let shader = pipeline(&raw const (*lib).shader_fill_buffer);
            commands.set_layout(shader.layout());
            commands.set_pipeline(shader);
            commands.bind_write(0, residuals.location());
            let words = ((*(*dec).idct_residuals).size / size_of::<[u32; 4]>()) as c_int;
            let value = [words, 0];
            commands.set_inline_constants(1, &value, 0);
            commands.dispatch(((value[0] + 63) / 64) as u32, 1, 1);
            perf_gpu_mark(dec, AV1_PERF_GPU_RESIDUAL_CLEAR);
            td.residual_init_clear = 1;
            need_residual_barrier = true;
        }

        let shader = pipeline(&raw const (*lib).shader_idct_sort);
        commands.set_layout(shader.layout());
        commands.set_pipeline(shader);
        // The records of the tiles: the pool of all frames, where a tile's records are in pieces: a
        // dispatch for each run of pieces that follow each other.
        commands.bind_read(0, buffer(td.parse_buffers[super::pool::Kind::Txb as usize]).location());
        commands.bind_write(1, blocks.location());
        for (index, tile) in tiles.iter().enumerate() {
            for run in joined_runs(&td.pieces[index].txb) {
                let sort = alloc_constants::<SortData>(&mut td.command_buffer);
                for tx_type in 0..TX_TYPES {
                    (*sort.data).offsets[tx_type][0] = tile.idct_blocks_sizes[tx_type];
                }
                (*sort.data).count = run.count;
                (*sort.data).src_offset = run.first;
                if run.count != 0 {
                    commands.bind_constants(2, sort.location);
                    commands.dispatch(((run.count + 63) / 64) as u32, 1, 1);
                }
            }
        }
        if need_residual_barrier {
            commands.write_barrier2(blocks, residuals);
        } else {
            commands.write_barrier(blocks);
        }
        perf_gpu_mark(dec, AV1_PERF_GPU_IDCT_SORT);

        let frame = alloc_constants::<IdctFrameData>(&mut td.command_buffer);
        for plane in 0..3 {
            (*frame.data).planes[plane][0] = frame_buffer.planes[plane].res_stride;
            (*frame.data).planes[plane][1] = frame_buffer.planes[plane].res_offset;
        }
        (*frame.data).bitdepth = td.bitdepth;

        let starts = tiles[0].idct_blocks_sizes;
        td.idct_tx_starts = starts;
        td.idct_frame_cb_location = frame.location;

        commands.set_layout(layout(&raw const (*lib).sig_idct));
        commands.bind_read(0, blocks.location());
        commands.bind_write(1, residuals.location());
        commands.bind_write(2, buffer(td.parse_buffers[super::pool::Kind::Coefs as usize]).location());
        commands.bind_constants(4, frame.location);
        // The first tile has the start of each type in the list of the frame.
        for tx_type in 0..TX_TYPES {
            let offset = starts[tx_type];
            let count = starts[tx_type + 1] - offset;
            if count == 0 {
                continue;
            }
            let shader = pipeline(&raw const (*lib).shader_idct[tx_type]);
            if matches!(tx_type as c_int, TX_4X4 | TX_4X8 | TX_8X4 | TX_8X8) {
                // One lane per block; the small kernels encode the scans in the shader.
                commands.set_pipeline(shader);
                commands.set_inline_constants(5, &[offset, count], 0);
                commands.dispatch((count as u32).div_ceil(64), 1, 1);
                continue;
            }
            // The lossless transform is 4x4.
            let tx_size = if tx_type == TX_SIZES_ALL as usize { TX_4X4 as usize } else { tx_type };
            let block_w = TX_SIZE_WIDE[tx_size] as usize;
            let block_h = TX_SIZE_HIGH[tx_size] as usize;
            let block_size = block_w * block_h;
            let work_items_per_block = block_w.min(block_h) as c_int;

            // The three scan orders of the size, as the shader indexes them. A transform 64 wide has
            // coefficients in its left half only; its scans count in rows of 32 and become rows of 64.
            let scans = alloc_constant_bytes(&mut td.command_buffer, block_size * 3 * 4);
            let gpu_scans = scans.data.cast::<c_int>();
            for (order, &scan) in super::libaom::IDCT_SCANS[tx_size].iter().enumerate() {
                let dst = std::slice::from_raw_parts_mut(gpu_scans.add(order * block_size), block_size);
                // A transform with a side of 64 has coefficients in 32 of them only, and its scan is that
                // of the 32: shorter than the block. The positions behind the scan are never reached
                // (the C read them from whatever followed the table); they are 0 here.
                dst.fill(0);
                if block_w > 32 {
                    for (dst, &position) in dst.iter_mut().zip(scan) {
                        let position = c_int::from(position);
                        *dst = (position & 31) + (position / 32) * 64;
                    }
                } else {
                    for (dst, &position) in dst.iter_mut().zip(scan) {
                        *dst = c_int::from(position);
                    }
                }
            }

            let inline = [offset, work_items_per_block * count];
            commands.set_pipeline(shader);
            commands.bind_constants(3, scans.location);
            commands.set_inline_constants(5, &inline, 0);
            commands.dispatch(((inline[1] + 63) / 64) as u32, 1, 1);
        }
        commands.write_barrier(residuals);
        perf_gpu_mark(dec, AV1_PERF_GPU_IDCT);
        stage_dump_residuals(dec, c"idct");
    }
}

/// Zeroes the residuals written by this frame right after `prediction_run_all` finishes reading them,
/// so `idct_residuals` returns to all zeros without stalling `idct` or touching skipped areas.
/// Runs concurrently with the post-prediction filter stages (`loopfilter`, `cdef`, `loop_rest`);
/// [`finish_residual_clear_barrier`] closes the UAV barrier before submission.
///
/// # Safety
/// `dec` is the decoder while it records a frame whose `idct_run` and `prediction_run_all` have run.
pub(crate) unsafe fn clear_residuals_post_pred(dec: *mut Av1Core) {
    if residual_clear_legacy() {
        return;
    }
    // SAFETY: the caller's contract.
    unsafe {
        let td = &mut *(*dec).curr_frame_data;
        let starts = td.idct_tx_starts;
        let total_blocks = starts[TX_TYPES];
        if total_blocks <= 0 {
            return;
        }
        let frame_buffer = &*td.frame_buffer;
        let active_words = super::residual::clear_work_items(
            (*(*dec).idct_residuals).size,
            frame_buffer.planes[1].res_offset,
            frame_buffer.planes[2].res_offset,
        );
        let active_bytes = active_words as usize * size_of::<[u32; 4]>();

        let mut tx_bytes = 0usize;
        for tx_type in 0..TX_TYPES {
            let count = (starts[tx_type + 1] - starts[tx_type]).max(0) as usize;
            if count == 0 {
                continue;
            }
            let tx_size = if tx_type == TX_SIZES_ALL as usize { TX_4X4 as usize } else { tx_type };
            tx_bytes += count * TX_SIZE_WIDE[tx_size] as usize * TX_SIZE_HIGH[tx_size] as usize * 2;
        }

        let lib = (*dec).shader_lib;
        let commands = commands(dec);
        let residuals = buffer((*dec).idct_residuals);
        let blocks = buffer((*dec).idct_blocks);

        // Wait for prediction's reads of `residuals` before overwriting them with zeros.
        commands.write_barrier(residuals);

        if tx_bytes * 2 <= active_bytes {
            let shader = pipeline(&raw const (*lib).shader_residual_clear_blocks);
            commands.set_layout(shader.layout());
            commands.set_pipeline(shader);
            commands.bind_read(0, blocks.location());
            commands.bind_write(1, residuals.location());
            commands.bind_constants(2, td.idct_frame_cb_location);
            for tx_type in 0..TX_TYPES {
                let offset = starts[tx_type];
                let count = starts[tx_type + 1] - offset;
                if count <= 0 {
                    continue;
                }
                let tx_size = if tx_type == TX_SIZES_ALL as usize { TX_4X4 as usize } else { tx_type };
                let tx_w = TX_SIZE_WIDE[tx_size] as u32;
                let tx_h = TX_SIZE_HIGH[tx_size] as u32;
                let (col_shift, lanes_per_block) = if tx_w == 4 {
                    (0u32, (tx_h / 8).max(1))
                } else {
                    let col_shift = (tx_w >> 3).trailing_zeros();
                    let stores_per_block = tx_h << col_shift;
                    (col_shift, (stores_per_block >> 3).max(1))
                };
                let lanes_shift = lanes_per_block.trailing_zeros();
                let total_lanes = (count as u32) << lanes_shift;
                let packed_dims = tx_w | (tx_h << 8) | (col_shift << 16);
                commands.set_inline_constants(3, &[offset as u32, total_lanes, lanes_shift, packed_dims], 0);
                commands.dispatch(total_lanes.div_ceil(64), 1, 1);
            }
        } else {
            let shader = pipeline(&raw const (*lib).shader_fill_buffer);
            commands.set_layout(shader.layout());
            commands.set_pipeline(shader);
            commands.bind_write(0, residuals.location());
            let value = [active_words, 0];
            commands.set_inline_constants(1, &value, 0);
            commands.dispatch(((active_words + 63) / 64) as u32, 1, 1);
        }

        td.residual_clear_pending = 1;
        perf_gpu_mark(dec, AV1_PERF_GPU_RESIDUAL_CLEAR);
    }
}

/// Closes the UAV barrier for the overlapped post-prediction residual clear before submission and
/// commits `residuals_initialized` once the first frame's full init clear is about to be submitted.
///
/// # Safety
/// `dec` is the decoder right before `commit_command_buffer`.
pub(crate) unsafe fn finish_residual_clear_barrier(dec: *mut Av1Core) {
    // SAFETY: the caller's contract.
    unsafe {
        let td = &mut *(*dec).curr_frame_data;
        if td.residual_clear_pending != 0 {
            let commands = commands(dec);
            let blocks = buffer((*dec).idct_blocks);
            let residuals = buffer((*dec).idct_residuals);
            commands.write_barrier2(blocks, residuals);
            td.residual_clear_pending = 0;
        }
        if td.residual_init_clear != 0 {
            (*dec).residuals_initialized = 1;
            td.residual_init_clear = 0;
        }
    }
}
