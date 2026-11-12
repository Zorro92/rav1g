// SPDX-License-Identifier: BSD-2-Clause
//
// This source code is subject to the terms of the BSD 2 Clause License and the Alliance for Open Media
// Patent License 1.0. If the BSD 2 Clause License was not distributed with this source code in the LICENSE
// file, you can obtain it at www.aomedia.org/license/software. If the Alliance for Open Media Patent
// License 1.0 was not distributed with this source code in the PATENTS file, you can obtain it at
// www.aomedia.org/license/patent.

//! The schedule of `intra_v2.hlsl` (test player flag -intrav2). From `libav1/dx/intra_v2_sched.h`, which
//! stays in the tree for the host check (`testing/intra_check`).
//!
//! The original path gives every intra block a wavefront iteration of the whole frame and runs one
//! dispatch pair per iteration. Here the frame is cut into 64x64 luma regions (16x16 cells of 4x4
//! samples, plus the 8x8 chroma cells below them). A block gets
//!
//! - a step: the dispatch round. All blocks of a region with the same step are handled by one thread
//!   group. A block's step is higher than the step of every block it reads from another region, so the
//!   host needs one barrier per step and groups never wait for each other;
//! - a pass: the round inside the thread group, higher than the pass of every block it reads in its own
//!   group. A filter intra block takes one pass per anti-diagonal of its cells.
//!
//! Blocks are placed in decode order, like the iteration grid of the original.
//!
//! The schedule itself is the C++ struct `IntraV2Sched` of a pipeline slot: the cell map and the region
//! list the shader reads (two words per 4x4 cell, luma cells first, then the chroma cells: block index in
//! its type and the type; step, last pass of the block and stamp; one 64-bit entry per thread group), and
//! the current entry of every region on the CPU side. Tiles are decoded on several threads: cells and
//! regions of different tiles are disjoint, the region list is appended to through an atomic counter.

use std::ffi::c_int;
use std::sync::atomic::{AtomicI32, Ordering};

use super::types::{IntraV2Dispatch, IntraV2Sched};

/// Block type of filter intra blocks (0..8: 64x64 .. 4x4 by area).
const FILTER_BUCKET: u32 = 9;
/// Passes of one thread group; a region with a longer chain of blocks goes on in a later step. The last
/// pass of a block is stored in 8 bits.
const MAX_PASSES: u32 = 250;

/// What a place of the schedule reads of a cell: its second word, when the cell was written for this
/// frame.
#[inline(always)]
fn written(word: u32, stamp: u32) -> bool {
    word & 0xff00_0000 == stamp
}

/// `intra_v2_place`: places one transform block. `(cx, cy)` and the sizes are in 4x4 cells of the
/// block's plane. The dependencies are the cells the original scans in its iteration grid: `above_count`
/// cells of the row above (one more to the left with `above_left`), `left_count` cells of the column to
/// the left, and for CFL the luma cells under the block. `index` is the block's index in its type (and
/// tile); U and V blocks are placed once, with the index of U.
///
/// # Safety
/// `s` is the schedule of the frame with its cell map, regions and list; the block and the cells it
/// depends on are inside the frame, and no other thread places blocks of the same tile.
#[expect(clippy::too_many_arguments, reason = "the arguments of the C++ function")]
pub(crate) unsafe fn place(
    s: &IntraV2Sched,
    tile_base: u32,
    is_uv: bool,
    cx: c_int,
    cy: c_int,
    w_log: c_int,
    h_log: c_int,
    above_left: c_int,
    above_count: c_int,
    left_count: c_int,
    cfl: bool,
    bucket: u32,
    index: u32,
) {
    // SAFETY: the caller's contract. The words are read and written through pointers, as several tile
    // threads work on the map at once, each on its own cells.
    unsafe {
        let uv = c_int::from(is_uv);
        let shift = 4 - uv; // log2 of the cells per region row
        let stride = (s.map_stride >> uv) as isize;
        let map = if is_uv { s.cells.offset(2 * s.map_uv_offset as isize) } else { s.cells };
        let rx = cx >> shift;
        let ry = cy >> shift;
        let region = s.regions.offset(2 * (rx + ry * s.regions_stride) as isize);
        let stamp = s.stamp << 24;
        let open = written(*region.add(1), stamp);
        let cur = *region.add(1) & 0xffff;
        let mut need_step = 0u32;
        let mut need_pass = 0u32;
        // A cell the block reads: in its own region and step it sets the pass, in another region the step.
        let mut depend = |word: u32, same_region: bool| {
            if !written(word, stamp) {
                return;
            }
            let step = word & 0xffff;
            if same_region {
                if step == cur {
                    need_pass = need_pass.max(((word >> 16) & 255) + 1);
                }
            } else {
                need_step = need_step.max(step + 1);
            }
        };

        if above_count + above_left > 0 {
            let row = map.wrapping_offset(2 * ((cy - 1) as isize * stride) + 1);
            let same_row = ((cy - 1) >> shift) == ry;
            for x in cx - above_left..cx + above_count {
                depend(*row.offset(2 * x as isize), same_row && (x >> shift) == rx);
            }
        }
        if left_count > 0 {
            let same_col = ((cx - 1) >> shift) == rx;
            for y in cy..cy + left_count {
                depend(*map.offset(2 * (y as isize * stride + cx as isize - 1) + 1), same_col && (y >> shift) == ry);
            }
        }
        if cfl {
            // The luma cells under a chroma block are in the same region.
            for y in (cy << 1)..(cy << 1) + (2 << h_log) {
                let row = s.cells.offset(2 * (y as isize * s.map_stride as isize + (cx << 1) as isize) + 1);
                for x in 0..(2 << w_log) {
                    let word = *row.offset(2 * x as isize);
                    if written(word, stamp) && (word & 0xffff) == cur {
                        need_pass = need_pass.max(((word >> 16) & 255) + 1);
                    }
                }
            }
        }

        let duration = if bucket == FILTER_BUCKET { ((1 << w_log) + (1 << h_log) - 2) as u32 } else { 0 };
        let mut step = cur;
        let mut pass = need_pass;
        let mut new_entry = false;
        if !open || need_step > cur {
            step = need_step;
            pass = 0;
            new_entry = true;
        } else if pass > 0 && pass + duration >= MAX_PASSES {
            step = cur + 1;
            pass = 0;
            new_entry = true;
        }
        step = step.min(0xffff); // more steps than list entries of any real frame
        if new_entry {
            let entry = AtomicI32::from_ptr(s.list_count.cast()).fetch_add(1, Ordering::SeqCst) as u32;
            *region = entry;
            *region.add(1) = step | stamp;
            *s.list.add(entry as usize) =
                u64::from(tile_base | ((rx as u32) << 24)) | (u64::from(ry as u32 | (step << 16)) << 32);
        }
        let last_pass = pass + duration;
        let entry = s.list.add(*region as usize);
        if u64::from(last_pass) > ((*entry >> 40) & 255) {
            *entry = (*entry & !(0xffu64 << 40)) | (u64::from(last_pass) << 40);
        }

        let word0 = index | (bucket << 24);
        let word1 = step | (last_pass << 16) | stamp;
        for y in cy..cy + (1 << h_log) {
            let cell = map.offset(2 * (y as isize * stride + cx as isize));
            for x in 0..(1isize << w_log) {
                *cell.offset(2 * x) = word0;
                *cell.offset(2 * x + 1) = word1;
            }
        }
    }
}

/// `intra_v2_sort`: orders the region list by step, then by number of passes, for [`next_dispatch`].
pub(crate) fn sort(list: &mut [u64]) {
    list.sort_unstable();
}

/// `intra_v2_next_dispatch`: cuts the sorted list into dispatches, from the entry `first` on: all thread
/// groups of a dispatch run the pass count of its busiest region, so regions of one step with very
/// different pass counts are dispatched separately. `None` at the end. `last_of_step`: a barrier has to
/// follow.
pub(crate) fn next_dispatch(list: &[u64], first: usize) -> Option<IntraV2Dispatch> {
    let step_of = |entry: u64| (entry >> 48) as u32;
    let passes_of = |entry: u64| ((entry >> 40) as u32) & 255;
    let head = *list.get(first)?;
    let step = step_of(head);
    let limit = 2 * passes_of(head) + 6;
    let mut end = first + 1;
    while end < list.len() && step_of(list[end]) == step && passes_of(list[end]) < limit {
        end += 1;
    }
    Some(IntraV2Dispatch {
        first: first as c_int,
        count: (end - first) as c_int,
        step,
        passes: passes_of(list[end - 1]) + 1,
        last_of_step: c_int::from(end >= list.len() || step_of(list[end]) != step),
    })
}
