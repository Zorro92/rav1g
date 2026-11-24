// SPDX-License-Identifier: BSD-2-Clause
//
// This source code is subject to the terms of the BSD 2 Clause License and the Alliance for Open Media
// Patent License 1.0. If the BSD 2 Clause License was not distributed with this source code in the LICENSE
// file, you can obtain it at www.aomedia.org/license/software. If the Alliance for Open Media Patent
// License 1.0 was not distributed with this source code in the PATENTS file, you can obtain it at
// www.aomedia.org/license/patent.

//! The mode info grid built by the GPU (docs/phase2-rav1d.md, "The block lists on the GPU: a design",
//! step C): with
//! `-parseopt gpugrid` (`AV1_PARSER_OPTION_GPU_GRID`, the default) a pass in front of the frame's stages
//! (`mi_grid_build.hlsl`) builds the grid of the slot, the one every stage reads, from the frame's mode
//! info records, and the sink writes none (`SinkOptions::gpu_grid`): the stores of the grid are off the
//! parse threads. No thread of the CPU reads the grid in such a decoder. The stage dump `mi_grid`, taken
//! behind the pass, is held against the dump of a decoder whose sink writes the grid. (Until interface
//! version 3, `-parseopt gridcheck` had the pass write a second grid that the completion thread held
//! against the sink's; it is retired: with the default `gpugrid` there is no grid of the parse to hold
//! against.)

use std::ffi::c_int;

use super::core::{buffer, commands, pipeline};
use super::log::log;
use super::types::Av1Core;
use crate::gpu::BufferState;
use crate::sys::{AV1_COMMON, MB_MODE_INFO, MI_GRID_BASE, MiGridEntry};

// The fields of a record the pass reads, by their place (MI_POSITION_WORD and SB_TYPE_WORD in
// mode_info.h), and the size of a grid entry.
const _: () = {
    assert!(std::mem::offset_of!(MB_MODE_INFO, mi_row) == 40);
    assert!(std::mem::offset_of!(MB_MODE_INFO, mi_col) == 42);
    assert!(std::mem::offset_of!(MB_MODE_INFO, sb_type) == 48);
    assert!(size_of::<MiGridEntry>() == 4);
};

/// Records the pass that builds the grid of the frame into the grid of the slot, in front of the frame's
/// stages, in a decoder whose parse writes none (`Av1Core::gpu_grid`). Nothing in any other decoder.
///
/// # Safety
/// `dec` is the decoder while it records a frame whose tiles are parsed; `cm` is the frame's.
pub(crate) unsafe fn record(dec: *mut Av1Core, cm: *const AV1_COMMON) {
    // SAFETY: the caller's contract.
    unsafe {
        if (*dec).gpu_grid == 0 {
            return;
        }
        let td = &mut *(*dec).curr_frame_data;
        let target = td.mode_info_grid;
        if target.is_null() || td.mi_runs.is_null() {
            return;
        }
        let cm = &*cm;
        // The runs of the frame's records, each with the records of the runs in front of it.
        let tile_count = usize::try_from(td.tile_count).unwrap_or(0);
        let runs_buffer = &*td.mi_runs;
        let room = runs_buffer.size / (2 * size_of::<u32>());
        let words = runs_buffer.host_ptr.cast::<u32>();
        let mut runs = 0usize;
        let mut records = 0u32;
        for pieces in td.pieces.iter().take(tile_count) {
            for run in super::frame::joined_runs(&pieces.mode_info) {
                if runs == room || run.first < 0 {
                    // More runs than the pool has chunks, which cannot be: the frame would be decoded with
                    // the grid of another frame.
                    log(true, "GPUGRID a frame has more runs of mode info records than the pool has chunks");
                    return;
                }
                words.add(2 * runs).write(run.first as u32);
                words.add(2 * runs + 1).write(records);
                records += run.count as u32;
                runs += 1;
            }
        }
        if runs == 0 || records == 0 {
            return;
        }
        let grid = buffer(target);
        let commands = commands(dec);
        let shader = pipeline(&raw const (*(*dec).shader_lib).shader_mi_grid_build);
        commands.transition(grid, BufferState::Read, BufferState::ReadWrite);
        commands.set_layout(shader.layout());
        commands.set_pipeline(shader);
        commands.bind_read(0, buffer(td.parse_buffers[super::pool::Kind::ModeInfo as usize]).location());
        commands.bind_read(1, buffer(td.mi_runs).location());
        commands.bind_write(2, grid.location());
        // run_count, records, mi_stride, mi_cols, mi_rows, mi_addr_base, record_bytes
        let inline = [
            runs as c_int,
            records as c_int,
            cm.mi_stride,
            cm.mi_cols,
            cm.mi_rows,
            MI_GRID_BASE as c_int,
            size_of::<MB_MODE_INFO>() as c_int,
        ];
        commands.set_inline_constants(3, &inline, 0);
        // A thread a record.
        commands.dispatch(records.div_ceil(64), 1, 1);
        commands.transition(grid, BufferState::ReadWrite, BufferState::Read);
    }
}
