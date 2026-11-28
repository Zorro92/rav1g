// SPDX-License-Identifier: BSD-2-Clause
//
// This source code is subject to the terms of the BSD 2 Clause License and the Alliance for Open Media
// Patent License 1.0. If the BSD 2 Clause License was not distributed with this source code in the LICENSE
// file, you can obtain it at www.aomedia.org/license/software. If the Alliance for Open Media Patent
// License 1.0 was not distributed with this source code in the PATENTS file, you can obtain it at
// www.aomedia.org/license/patent.

//! The buffers of the stages that hold what the blocks of one frame need, sized for the frames and not
//! for the worst case (docs/memory.md): the sorted transform blocks (`idct_blocks`), the two lists
//! `gen_pred_blocks` writes (`prediction_blocks`, `prediction_blocks_warp`), and the block index list of
//! each slot (`gen_mi_block_indexes`).
//!
//! One command list uses the first three at a time, and how many records a frame writes into each is
//! known on the CPU when its tiles are parsed. So a decoder is created with a part of the worst case
//! ([`initial_bytes`]), and the frame that needs more gets a larger buffer before it is recorded
//! ([`for_frame`]): the thread that records the frames waits until the queue has run everything that
//! was submitted, the old buffer is released and the new one created. A buffer never gets smaller, and
//! never larger than the worst case the decoder was created for, which is what it always was before.
//!
//! The block index list is written by the parse, tile by tile, at places that used to be laid out for
//! the worst case of every tile. The tiles now write lists of their own in host memory
//! (`TilePieces::indexes`), which the recording copies into the slot's buffer one behind the other
//! (`prediction_gen_blocks`); the buffer grows like the others, without the wait: only the frame of the
//! slot reads it.
//!
//! The growth is held against the application's budget like a palette buffer: what it adds and a margin
//! of 16 MiB have to fit what the application has left (`memory_available_cb`), or the frame is a decode
//! error and nothing is allocated ("MEMORY REFUSED"). The headroom of the creation does not refuse it.

use std::ffi::c_int;

use windows::Win32::Graphics::Direct3D12::{ID3D12CommandQueue, ID3D12Device};

use super::create::to_mib;
use super::log::log;
use super::pool::Kind;
use super::prediction::pred_block_counts;
use super::types::{Av1Core, GpuBufferObject, av1_frame_thread_data};
use crate::gpu::d3d12::com_ref;
use crate::gpu::{Device, Event, Queue};

/// The buffers that grow, in the order of `Av1Core::stage_worst`.
pub(crate) const STAGE_BUFFERS: usize = 5;
pub(crate) const IDCT_BLOCKS: usize = 0;
pub(crate) const PRED_BLOCKS: usize = 1;
pub(crate) const PRED_BLOCKS_WARP: usize = 2;
pub(crate) const INDEX_LIST: usize = 3;
pub(crate) const GRID_RUNS: usize = 4;
/// The bytes of a record of each.
pub(crate) const RECORD: [usize; STAGE_BUFFERS] = [16, 16, 48, 4, 2 * size_of::<u32>()];

/// The part of the worst case a decoder is created with.
const INITIAL_DIVISOR: usize = 4;
/// Records behind the last one a frame writes: a stage that reads the list never reads behind the buffer
/// when its last group of threads is not full.
const SLACK_RECORDS: usize = 64;
/// A buffer that grows gets an eighth more than the frame needs, in steps of this many bytes.
const GROWTH_STEP: usize = 64 << 10;

/// `AV1_MEMORY_GUARD_MARGIN_BUFFER` of `aom/aom_decoder.h`: what a buffer that is created after the
/// decoder must leave of what the application has left.
pub(crate) const MEMORY_GUARD_MARGIN_BUFFER: u64 = 16 << 20;

/// The bytes a decoder is created with for a buffer whose worst case is `worst` bytes.
pub(crate) fn initial_bytes(worst: usize) -> usize {
    worst.div_ceil(INITIAL_DIVISOR)
}

/// The bytes a buffer of `current` bytes has to have for a frame of `records` records of `record` bytes,
/// when that is more than it has: an eighth above the frame in steps of [`GROWTH_STEP`], at most the worst
/// case, and never less than the frame's records.
fn grown_bytes(current: usize, records: usize, record: usize, worst: usize) -> Option<usize> {
    let exact = records * record;
    let needed = ((records + SLACK_RECORDS) * record).min(worst.max(exact));
    if needed <= current {
        return None;
    }
    let step = |bytes: usize| bytes.div_ceil(GROWTH_STEP) * GROWTH_STEP;
    Some(step(needed + needed / 8).min(worst).max(needed))
}

/// The records the frame on `td` writes into each buffer: its transform blocks, the two lists of
/// `gen_pred_blocks`, and the entries of the block index lists of its tiles.
///
/// # Safety
/// `td` is the slot of a frame whose tiles are parsed and whose stages are not recorded yet.
unsafe fn frame_records(td: &av1_frame_thread_data) -> [usize; STAGE_BUFFERS] {
    // SAFETY: the caller's contract: the tiles of the frame, with the counts of their lists.
    unsafe {
        let tiles = std::slice::from_raw_parts(td.tile_data, td.tile_count.max(0) as usize);
        let transform: c_int = tiles.iter().map(|tile| tile.idct_blocks_sizes.iter().sum::<c_int>()).sum();
        let (blocks, warp) = pred_block_counts(tiles);
        let indexes: c_int = tiles.iter().map(|tile| tile.gen_index_ptr.max(0)).sum();
        let mut records = [0; STAGE_BUFFERS];
        records[..4].copy_from_slice(&[transform, blocks, warp, indexes].map(|count| count.max(0) as usize));
        records
    }
}

/// Waits until the queue has run every command list that was submitted.
///
/// # Safety
/// `dec` is the decoder, on the thread that submits its command lists.
unsafe fn wait_for_queue(dec: *mut Av1Core) -> bool {
    // SAFETY: the caller's contract.
    unsafe {
        let (Some(device), Some(queue)) = (
            com_ref::<_, ID3D12Device>(&(*dec).compute.device),
            com_ref::<_, ID3D12CommandQueue>(&(*dec).compute.queue),
        ) else {
            return false;
        };
        let (Ok(fence), Ok(event)) = (Device::new(device).create_fence(false), Event::new()) else {
            return false;
        };
        Queue::new(queue).signal(&fence, 1);
        fence.set_event(1, &event);
        event.wait();
        Event::close(event.into_raw());
        true
    }
}

/// The frame on the slot `td`, whose tiles are parsed, is about to be recorded: every buffer of the
/// stages holds what the frame writes into it, or is made larger first. Returns false when the frame
/// does not get what it needs (the budget of the application, or the allocation; a "MEMORY REFUSED" or
/// "MEMORY FAILED" line says which): the frame is then a decode error and must not be recorded.
///
/// # Safety
/// `dec` is the decoder and `td` the slot of a frame of it whose tiles are parsed; the thread that
/// records and submits the frames, with no frame half recorded.
pub(crate) unsafe fn for_frame(dec: *mut Av1Core, td: *const av1_frame_thread_data) -> bool {
    // SAFETY: the caller's contract, for all of this function.
    unsafe {
        let mut records = frame_records(&*td);
        // At creation the run list holds at least one entry for each mode info chunk. After
        // a pool growth, make room for the frame's pieces before writing the run list.
        if !(*td).mi_runs.is_null() && (*td).parse_buffers[Kind::ModeInfo as usize] != (*dec).mode_info_pool {
            records[GRID_RUNS] =
                (&(*td).pieces)[..(*td).tile_count as usize].iter().map(|tile| tile.mode_info.len()).sum();
        }
        let objects: [*mut GpuBufferObject; STAGE_BUFFERS] = [
            (*dec).idct_blocks,
            (*dec).prediction_blocks,
            (*dec).prediction_blocks_warp,
            (*td).gen_mi_block_indexes,
            (*td).mi_runs,
        ];
        let mut waited = false;
        for index in 0..STAGE_BUFFERS {
            let object = objects[index];
            if object.is_null() {
                debug_assert_eq!(records[index], 0);
                continue;
            }
            let (current, worst) = ((*object).size, (*dec).stage_worst[index]);
            if index == GRID_RUNS && records[index] * RECORD[index] <= current {
                continue;
            }
            let Some(size) = grown_bytes(current, records[index], RECORD[index], worst) else {
                continue;
            };
            let name = std::ffi::CStr::from_ptr((*object).name).to_string_lossy();
            let frame = (*td).frame_number;
            let added = (size - current) as u64;
            // The allocator after the creation: the decode thread creates palette buffers with it.
            let _memory = (*dec).memory_lock.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some(available_cb) = (*dec).memory_available_cb {
                let available = available_cb((*dec).memory_available_priv);
                if available >= 0 && added + MEMORY_GUARD_MARGIN_BUFFER > available as u64 {
                    log(
                        true,
                        &format!(
                            "MEMORY REFUSED buffer {name} grown for frame {frame}, which writes {} records: {:.1} \
                             MiB more ({:.1} to {:.1} MiB) and the margin of {:.1} MiB do not fit the {:.1} MiB the \
                             application has left for the decoder. The frame is a decode error; nothing was \
                             allocated and the D3D12 device is intact",
                            records[index],
                            to_mib(added),
                            to_mib(current as u64),
                            to_mib(size as u64),
                            to_mib(MEMORY_GUARD_MARGIN_BUFFER),
                            to_mib(available as u64)
                        ),
                    );
                    return false;
                }
            }
            // Other frames in flight use shared buffers. The index list and the runs belong to this slot.
            if !waited && !matches!(index, INDEX_LIST | GRID_RUNS) {
                if !wait_for_queue(dec) {
                    log(
                        true,
                        &format!("MEMORY FAILED buffer {name} grown for frame {frame}: no fence to wait for the queue"),
                    );
                    return false;
                }
                waited = true;
            }
            let memory = &mut *(*dec).memory;
            let device = com_ref::<_, ID3D12Device>(&(*dec).compute.device);
            // A failure is logged by the allocator as "MEMORY FAILED buffer <name>, grown for a frame".
            if !memory.resize_buffer(device, object, size) {
                return false;
            }
            (*dec).stage_growths[index] += 1;
            log(
                false,
                &format!(
                    "MEMORY buffer {name} grown for frame {frame}, which writes {} records: {:.1} to {:.1} MiB of \
                     the {:.1} MiB of the worst case; device buffers now {:.1} MiB ({} bytes)",
                    records[index],
                    to_mib(current as u64),
                    to_mib(size as u64),
                    to_mib(worst as u64),
                    to_mib(memory.device_bytes() as u64),
                    memory.device_bytes()
                ),
            );
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const _: () =
        assert!(STAGE_BUFFERS == GRID_RUNS + 1 && PRED_BLOCKS_WARP == 2 && PRED_BLOCKS == 1 && IDCT_BLOCKS == 0);

    #[test]
    fn a_buffer_grows_only_for_a_frame_that_needs_more() {
        let worst = 12 << 20;
        let initial = initial_bytes(worst);
        assert_eq!(initial, 3 << 20);
        // A frame within the buffer, with the records behind its last one.
        assert_eq!(grown_bytes(initial, 100_000, 16, worst), None);
        assert_eq!(grown_bytes(initial, initial / 16 - SLACK_RECORDS, 16, worst), None);
        // One record more: an eighth above it, in steps.
        let grown = grown_bytes(initial, initial / 16 - SLACK_RECORDS + 1, 16, worst).unwrap();
        assert!(grown > initial + initial / 8 && grown <= initial + initial / 8 + GROWTH_STEP + 16);
        assert_eq!(grown % GROWTH_STEP, 0);
        // The grown buffer holds the frame, and stays.
        assert_eq!(grown_bytes(grown, initial / 16 - SLACK_RECORDS + 1, 16, worst), None);
    }

    #[test]
    fn the_worst_case_is_the_most() {
        let worst = 221_184; // idct_blocks of a decoder for 352x288
        let all = worst / 16;
        assert_eq!(grown_bytes(initial_bytes(worst), all, 16, worst), Some(worst));
        assert_eq!(grown_bytes(initial_bytes(worst), all - 10, 16, worst), Some(worst));
        assert_eq!(grown_bytes(worst, all, 16, worst), None);
        // A buffer that was lost (a failed allocation) is created again for the next frame.
        assert_eq!(grown_bytes(0, 0, 16, worst), Some(GROWTH_STEP));
        assert_eq!(grown_bytes(0, 0, 48, 1000), Some(1000));
    }
}
