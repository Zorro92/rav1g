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

use super::core::{
    alloc_constant_bytes, alloc_constants, buffer, commands, layout, perf_gpu_mark, pipeline, stage_dump_residuals,
};
use super::frame::joined_runs;
use super::tables::{TX_SIZE_HIGH, TX_SIZE_WIDE};
use super::types::Av1Core;
use crate::sys::parser::{
    AV1_PERF_GPU_IDCT, AV1_PERF_GPU_IDCT_SORT, AV1_PERF_GPU_RESIDUAL_CLEAR, TX_4X4, TX_SIZES_ALL,
};

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

        let shader = pipeline(&raw const (*lib).shader_fill_buffer);
        commands.set_layout(shader.layout());
        commands.set_pipeline(shader);
        commands.bind_write(0, residuals.location());
        let value = [((*(*dec).idct_residuals).size >> 4) as c_int, 0];
        commands.set_inline_constants(1, &value, 0);
        commands.dispatch(((value[0] + 63) / 64) as u32, 1, 1);
        perf_gpu_mark(dec, AV1_PERF_GPU_RESIDUAL_CLEAR);

        let shader = pipeline(&raw const (*lib).shader_idct_sort);
        commands.set_layout(shader.layout());
        commands.set_pipeline(shader);
        // The records of the tiles: the pool of all frames, where a tile's records are in pieces: a
        // dispatch for each run of pieces that follow each other.
        commands.bind_read(0, buffer((*dec).idct_blocks_pool).location());
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
        commands.write_barrier2(blocks, residuals);
        perf_gpu_mark(dec, AV1_PERF_GPU_IDCT_SORT);

        let frame = alloc_constants::<IdctFrameData>(&mut td.command_buffer);
        let frame_buffer = &*td.frame_buffer;
        for plane in 0..3 {
            (*frame.data).planes[plane][0] = frame_buffer.planes[plane].res_stride;
            (*frame.data).planes[plane][1] = frame_buffer.planes[plane].res_offset;
        }
        (*frame.data).bitdepth = td.bitdepth;

        commands.set_layout(layout(&raw const (*lib).sig_idct));
        commands.bind_read(0, blocks.location());
        commands.bind_write(1, residuals.location());
        commands.bind_write(2, buffer((*dec).idct_coefs).location());
        commands.bind_constants(4, frame.location);
        // The first tile has the start of each type in the list of the frame.
        let starts = tiles[0].idct_blocks_sizes;
        for tx_type in 0..TX_TYPES {
            let offset = starts[tx_type];
            let count = starts[tx_type + 1] - offset;
            if count == 0 {
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
            commands.set_pipeline(pipeline(&raw const (*lib).shader_idct[tx_type]));
            commands.bind_constants(3, scans.location);
            commands.set_inline_constants(5, &inline, 0);
            commands.dispatch(((inline[1] + 63) / 64) as u32, 1, 1);
        }
        commands.write_barrier(residuals);
        perf_gpu_mark(dec, AV1_PERF_GPU_IDCT);
        stage_dump_residuals(dec, c"idct");
    }
}
