// SPDX-License-Identifier: BSD-2-Clause
//
// This source code is subject to the terms of the BSD 2 Clause License and the Alliance for Open Media
// Patent License 1.0. If the BSD 2 Clause License was not distributed with this source code in the LICENSE
// file, you can obtain it at www.aomedia.org/license/software. If the Alliance for Open Media Patent
// License 1.0 was not distributed with this source code in the PATENTS file, you can obtain it at
// www.aomedia.org/license/patent.

//! The check of the mode info grid built by the GPU (`-parseopt gridcheck`,
//! `AV1_PARSER_OPTION_GPU_GRID_CHECK`): the first step of building the grid on the GPU instead of on the
//! parse threads (docs/phase2-rav1d.md, "The block lists on the GPU: a design", step C). A pass in front
//! of the frame's stages (`mi_grid_build.hlsl`) builds the grid from the frame's mode info records into a
//! second grid of the slot; when the frame is done, the completion thread holds that grid against the one
//! the sink wrote, unit by unit, and the decoder's release says how many units differed (`GRIDCHECK`).
//! Nothing else reads the second grid, so the frames are the same with and without the check.

use std::ffi::c_int;

use super::core::{buffer, commands, pipeline};
use super::log::log;
use super::types::{Av1Core, av1_frame_thread_data};
use crate::gpu::BufferState;
use crate::sys::{AV1_COMMON, MB_MODE_INFO, MI_GRID_BASE, MiGridEntry};

// The fields of a record the pass reads, by their place (MI_ROW_BYTE, MI_COL_BYTE, SB_TYPE_WORD of
// mi_grid_build.hlsl), and the size of a record and of an entry it assumes.
const _: () = {
    assert!(std::mem::offset_of!(MB_MODE_INFO, mi_row) == 160);
    assert!(std::mem::offset_of!(MB_MODE_INFO, mi_col) == 164);
    assert!(std::mem::offset_of!(MB_MODE_INFO, sb_type) == 188);
    assert!(size_of::<MiGridEntry>() == 4);
};

/// What the checks of a decoder found, for the line of its release. All zero is "nothing checked".
#[derive(Clone, Copy, Debug, Default)]
pub struct GridCheck {
    /// Frames whose grids were held against each other, the units of those frames, and the units whose
    /// entries differed.
    pub frames: u64,
    pub units: u64,
    pub differ: u64,
    /// The first unit that differed: the frame (counted from 1, 0: none), its row and column, and the
    /// entry of the parse and the entry of the GPU.
    pub first_frame: u64,
    pub first_row: c_int,
    pub first_col: c_int,
    pub first_parse: u32,
    pub first_gpu: u32,
}

/// Records the pass that builds the grid of the frame in the slot's second grid, in front of the frame's
/// stages, and keeps the frame's size for [`compare`]. Nothing when the decoder has no check.
///
/// # Safety
/// `dec` is the decoder while it records a frame whose tiles are parsed; `cm` is the frame's.
pub(crate) unsafe fn record(dec: *mut Av1Core, cm: *const AV1_COMMON) {
    // SAFETY: the caller's contract.
    unsafe {
        if (*dec).gpu_grid_check == 0 {
            return;
        }
        let td = &mut *(*dec).curr_frame_data;
        td.grid_check_frame[0] = 0;
        if td.mode_info_grid_gpu.is_null() || td.mi_runs.is_null() {
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
                    // More runs than the pool has chunks, which cannot be: the frame is not checked.
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
        let grid = buffer(td.mode_info_grid_gpu);
        let commands = commands(dec);
        let shader = pipeline(&raw const (*(*dec).shader_lib).shader_mi_grid_build);
        commands.transition(grid, BufferState::Read, BufferState::ReadWrite);
        commands.set_layout(shader.layout());
        commands.set_pipeline(shader);
        commands.bind_read(0, buffer((*dec).mode_info_pool).location());
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
        td.grid_check_frame = [1, cm.mi_stride, cm.mi_cols, cm.mi_rows];
    }
}

/// Holds the grid the GPU built for the slot's frame against the grid the parse wrote, unit by unit
/// inside the frame. The completion thread, after the frame's fence and before the slot's grids are
/// cleared or the slot goes back.
///
/// # Safety
/// `dec` is the decoder of the slot `td`, whose frame is done on the GPU.
pub(crate) unsafe fn compare(dec: *mut Av1Core, td: &mut av1_frame_thread_data) {
    let [checked, stride, cols, rows] = td.grid_check_frame;
    if checked == 0 {
        return;
    }
    td.grid_check_frame[0] = 0;
    // SAFETY: the caller's contract: both grids are mapped buffers of the slot with the frame's units, and
    // nothing writes them until the slot goes back.
    unsafe {
        let parse = (*td.mode_info_grid).host_ptr.cast::<MiGridEntry>().cast_const();
        let gpu = (*td.mode_info_grid_gpu).host_ptr.cast::<MiGridEntry>().cast_const();
        let stats = &mut (*dec).grid_check;
        stats.frames += 1;
        for row in 0..rows {
            for col in 0..cols {
                let at = (row * stride + col) as usize;
                let (a, b) = (*parse.add(at), *gpu.add(at));
                if a != b {
                    stats.differ += 1;
                    if stats.first_frame == 0 {
                        stats.first_frame = stats.frames;
                        (stats.first_row, stats.first_col, stats.first_parse, stats.first_gpu) = (row, col, a, b);
                    }
                }
            }
        }
        stats.units += (rows.max(0) as u64) * (cols.max(0) as u64);
    }
}

/// The line of the decoder's release: what the checks found. Nothing when the decoder has no check.
///
/// # Safety
/// `dec` is a decoder whose frames are done.
pub(crate) unsafe fn summary(dec: *mut Av1Core) {
    // SAFETY: the caller's contract.
    let (on, stats) = unsafe { ((*dec).gpu_grid_check != 0, (*dec).grid_check) };
    if !on {
        return;
    }
    let first = if stats.first_frame != 0 {
        format!(
            "; the first: frame {} row {} column {}, the parse 0x{:x}, the GPU 0x{:x}",
            stats.first_frame, stats.first_row, stats.first_col, stats.first_parse, stats.first_gpu
        )
    } else {
        String::new()
    };
    log(
        stats.differ != 0,
        &format!(
            "GRIDCHECK the mode info grid built by the GPU against the parse's: {} frames, {} units, {} differ{first}",
            stats.frames, stats.units, stats.differ
        ),
    );
}
