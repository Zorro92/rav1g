// SPDX-License-Identifier: BSD-2-Clause
//
// This source code is subject to the terms of the BSD 2 Clause License and the Alliance for Open Media
// Patent License 1.0. If the BSD 2 Clause License was not distributed with this source code in the LICENSE
// file, you can obtain it at www.aomedia.org/license/software. If the Alliance for Open Media Patent
// License 1.0 was not distributed with this source code in the PATENTS file, you can obtain it at
// www.aomedia.org/license/patent.

//! Intra step timing (`av1_set_intra_step_timing` of `dx/av1_perf.h`; the test flags -intrasteptime and
//! -intrastepmin; docs/test-player.md, "Intra step timing"): a GPU timestamp in front of the first
//! wavefront step of a frame and one behind the barrier of every step, with what the host's schedule put
//! into each step, handed to a callback after the frame's fence. Nothing here changes what is decoded,
//! and nothing of it exists while no callback is installed.
//!
//! Where the work goes, sized for the console: the timestamps are in the command list the steps already
//! use (one `EndQuery` behind each step's barrier, one `ResolveQueryData` behind the last step), the
//! readback (8 bytes a step) is read on the completion thread after the fence, and the counts of a step
//! are a few additions per transform block on the tile threads, made only while the switch is on. The
//! query heap and the readback buffer of a slot are created when the slot first times a frame, for the
//! steps of that frame rounded up to a power of two (at least 1024 steps: 8 KiB).

use std::ffi::{c_int, c_void};
use std::sync::RwLock;
use std::sync::atomic::{AtomicBool, Ordering};

use windows::Win32::Graphics::Direct3D12::{ID3D12CommandQueue, ID3D12Device};

use super::core::commands;
use super::types::{Av1Core, av1_frame_thread_data};
use crate::gpu::d3d12::com_ref;
use crate::gpu::{Device, Queue, Readback, TimestampQueries};
use crate::sys;

const KINDS: usize = sys::AV1_INTRA_KIND_COUNT as usize;
const LISTS: usize = sys::AV1_INTRA_STEP_LISTS as usize;

/// What the tile threads count of a step: transform blocks by plane and kind, and the longest chain of
/// filter intra (columns + rows - 1 of a block in 4x2 units).
#[derive(Clone, Copy, Default)]
pub(crate) struct StepCounts {
    blocks: [[u32; KINDS]; 2],
    filter_diagonals: u32,
}

/// The counts of a tile, by step; grown as the tile reaches later steps.
pub(crate) type TileSteps = Vec<StepCounts>;

/// What the switch says: the callback, its context, and which frames.
struct Config {
    callback: unsafe extern "C" fn(*mut c_void, *const sys::av1_intra_steps),
    context: usize,
    min_steps: c_int,
    frames: Vec<c_int>,
}

static CONFIG: RwLock<Option<Config>> = RwLock::new(None);
static ON: AtomicBool = AtomicBool::new(false);

fn config() -> std::sync::RwLockReadGuard<'static, Option<Config>> {
    CONFIG.read().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// `av1_set_intra_step_timing` (`dx/av1_perf.h`).
///
/// # Safety
/// `frames` is null or valid for `count` numbers; `cb` may be called with `private` from the completion
/// thread of any decoder until it is replaced.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn av1_set_intra_step_timing(
    cb: sys::av1_intra_steps_cb_fn_t,
    private: *mut c_void,
    min_steps: c_int,
    frames: *const c_int,
    count: c_int,
) {
    let frames = if frames.is_null() || count <= 0 {
        Vec::new()
    } else {
        // SAFETY: the caller's contract.
        unsafe { std::slice::from_raw_parts(frames, count as usize) }.to_vec()
    };
    let config = cb.map(|callback| Config { callback, context: private as usize, min_steps, frames });
    ON.store(config.is_some(), Ordering::SeqCst);
    *CONFIG.write().unwrap_or_else(|poisoned| poisoned.into_inner()) = config;
}

/// The switch is on: the tiles count their steps.
pub(crate) fn on() -> bool {
    ON.load(Ordering::Relaxed)
}

/// Whether a frame with `steps` steps is timed.
fn wanted(frame_number: c_int, steps: c_int) -> bool {
    config().as_ref().is_some_and(|config| {
        config.frames.contains(&frame_number) || (config.min_steps >= 0 && steps > config.min_steps)
    })
}

/// The step timing of a slot.
#[derive(Default)]
pub(crate) struct SlotSteps {
    /// The counts of the tiles of the slot's frame, one vector per tile; each tile thread writes the
    /// vector of its tile (`tile.perf_steps`), the recording thread reads them when the tiles are done.
    pub tiles: Vec<TileSteps>,
    queries: Option<TimestampQueries>,
    readback: Option<Readback>,
    capacity: u32,
    failed: bool,
    frequency: u64,
    /// The slot's frame is timed: its steps, whose times come with the readback.
    timed: bool,
    steps: Vec<sys::av1_intra_step>,
    frame_number: c_int,
    show_frame: c_int,
    intra_frame: c_int,
    bit_depth: c_int,
    tile_count: c_int,
}

/// Frame setup: the tiles' counts start empty while the switch is on (each keeps its memory from frame
/// to frame of the slot). Returns whether the tiles count.
pub(crate) fn setup_frame(td: &mut av1_frame_thread_data) -> bool {
    let steps = &mut td.step_time;
    steps.timed = false;
    if !on() {
        steps.tiles.clear();
        return false;
    }
    let tiles = usize::try_from(td.tile_count).unwrap_or(0);
    steps.tiles.truncate(tiles);
    for tile in &mut steps.tiles {
        tile.clear();
    }
    steps.tiles.resize_with(tiles, Vec::new);
    true
}

/// The counts of the tile `tile_id`, for `av1_tile_data::perf_steps`; null while the tiles do not count.
pub(crate) fn tile_counts(td: &mut av1_frame_thread_data, tile_id: usize) -> *mut c_void {
    td.step_time.tiles.get_mut(tile_id).map_or(std::ptr::null_mut(), |tile| std::ptr::from_mut(tile).cast())
}

/// The kind of a transform block of the wavefront (`AV1_INTRA_KIND_*`): `mode` as the tile has it (the
/// luma mode, or the chroma mode with 13 for CFL), the angle delta, and what else the block is.
pub(crate) fn kind(mode: u32, delta: i8, filter: bool, intrabc: bool, inter_intra: bool) -> usize {
    const NOMINAL: [i32; 9] = [0, 90, 180, 45, 135, 113, 157, 203, 67];
    let kind = if intrabc {
        sys::AV1_INTRA_KIND_INTRABC
    } else if inter_intra {
        sys::AV1_INTRA_KIND_INTERINTRA
    } else if filter {
        sys::AV1_INTRA_KIND_FILTER
    } else {
        match mode {
            0 => sys::AV1_INTRA_KIND_DC,
            1..=8 => {
                let angle = NOMINAL[mode as usize] + 3 * i32::from(delta);
                if angle == 90 || angle == 180 {
                    sys::AV1_INTRA_KIND_STRAIGHT
                } else if angle < 90 {
                    sys::AV1_INTRA_KIND_ZONE1
                } else if angle < 180 {
                    sys::AV1_INTRA_KIND_ZONE2
                } else {
                    sys::AV1_INTRA_KIND_ZONE3
                }
            }
            9..=11 => sys::AV1_INTRA_KIND_SMOOTH,
            12 => sys::AV1_INTRA_KIND_PAETH,
            _ => sys::AV1_INTRA_KIND_CFL,
        }
    };
    kind as usize
}

/// A tile thread puts a transform block of `plane` (0 luma, 1 chroma) and `kind` into step `iter`;
/// `filter_diagonals` is its chain of filter intra (0 for other blocks).
///
/// # Safety
/// `counts` is the tile's `perf_steps`: null, or its vector, which only this thread uses while the tile
/// is in parse.
#[inline]
pub(crate) unsafe fn count(counts: *mut c_void, iter: c_int, plane: usize, kind: usize, filter_diagonals: u32) {
    // SAFETY: the caller's contract.
    let Some(steps) = (unsafe { counts.cast::<TileSteps>().as_mut() }) else {
        return;
    };
    let Ok(at) = usize::try_from(iter) else {
        return;
    };
    if steps.len() <= at {
        steps.resize(at + 1, StepCounts::default());
    }
    let step = &mut steps[at];
    step.blocks[plane][kind] += 1;
    step.filter_diagonals = step.filter_diagonals.max(filter_diagonals);
}

/// The timestamps of the slot's frame, as a borrow of the heap: present while the frame is timed.
fn heap(steps: &SlotSteps) -> Option<&TimestampQueries> {
    if steps.timed { steps.queries.as_ref() } else { None }
}

/// In front of the first step of the frame: decides whether the frame is timed and, if it is, takes the
/// timestamp in front of its first step. `step_count` steps of the main path.
///
/// # Safety
/// `dec` is the decoder while it records the frame of its current slot.
pub(crate) unsafe fn begin(dec: *mut Av1Core, step_count: c_int, show_frame: c_int, intra_frame: c_int) {
    // SAFETY: the caller's contract.
    unsafe {
        let td = &mut *(*dec).curr_frame_data;
        let frame_number = td.frame_number;
        let bit_depth = if td.is_hbd != 0 { 10 } else { 8 };
        let tile_count = td.tile_count;
        let steps = &mut td.step_time;
        steps.timed = false;
        if !on() || step_count <= 0 || !wanted(frame_number, step_count) {
            return;
        }
        let needed = step_count as u32 + 1;
        if steps.capacity < needed && !steps.failed {
            // The heap and the buffer of the slot, for at least this frame's steps.
            let capacity = needed.next_power_of_two().max(1024);
            let device = com_ref::<_, ID3D12Device>(&(*dec).compute.device).map(Device::new);
            let frequency = com_ref::<_, ID3D12CommandQueue>(&(*dec).compute.queue)
                .and_then(|queue| Queue::new(queue).timestamp_frequency());
            let created = device.zip(frequency).and_then(|(device, frequency)| {
                let queries = device.create_timestamp_queries(capacity).ok()?;
                let readback = device.create_readback(u64::from(capacity) * 8).ok()?;
                Some((queries, readback, frequency))
            });
            match created {
                Some((queries, readback, frequency)) => {
                    steps.queries = Some(queries);
                    steps.readback = Some(readback);
                    steps.capacity = capacity;
                    steps.frequency = frequency;
                }
                None => {
                    steps.queries = None;
                    steps.readback = None;
                    steps.capacity = 0;
                    steps.failed = true;
                }
            }
        }
        if steps.capacity < needed {
            return;
        }
        steps.timed = true;
        steps.frame_number = frame_number;
        steps.show_frame = show_frame;
        steps.intra_frame = intra_frame;
        steps.bit_depth = bit_depth;
        steps.tile_count = tile_count;
        steps.steps.clear();
        steps.steps.resize(step_count as usize, sys::av1_intra_step::default());
        commands(dec).timestamp(heap(steps).expect("a timed frame has its heap"), 0);
    }
}

/// Behind the barrier of step `it`: its timestamp, and what the step dispatched (the records of its ten
/// lists, its thread groups and dispatches).
///
/// # Safety
/// As for [`begin`].
pub(crate) unsafe fn step(dec: *mut Av1Core, it: c_int, lists: &[c_int; LISTS], groups: c_int, dispatches: c_int) {
    // SAFETY: the caller's contract.
    unsafe {
        let steps = &mut (*(*dec).curr_frame_data).step_time;
        let Some(at) = usize::try_from(it).ok().filter(|&at| at < steps.steps.len()) else {
            return;
        };
        let Some(queries) = heap(steps) else {
            return;
        };
        commands(dec).timestamp(queries, it as u32 + 1);
        let record = &mut steps.steps[at];
        record.lists = *lists;
        record.groups = groups;
        record.dispatches = dispatches;
    }
}

/// Behind the last step: resolves the timestamps into the readback buffer, and adds the tiles' counts to
/// the steps.
///
/// # Safety
/// As for [`begin`]; the tiles of the frame are done.
pub(crate) unsafe fn end(dec: *mut Av1Core) {
    // SAFETY: the caller's contract.
    unsafe {
        let steps = &mut (*(*dec).curr_frame_data).step_time;
        let (Some(queries), Some(readback)) = (heap(steps), steps.readback.as_ref()) else {
            return;
        };
        commands(dec).resolve_timestamps(queries, steps.steps.len() as u32 + 1, readback);
        let SlotSteps { tiles, steps: records, .. } = steps;
        for tile in tiles.iter() {
            for (record, counts) in records.iter_mut().zip(tile) {
                for plane in 0..2 {
                    for kind in 0..KINDS {
                        record.blocks[plane][kind] += counts.blocks[plane][kind] as c_int;
                    }
                }
                record.filter_diagonals = record.filter_diagonals.max(counts.filter_diagonals as c_int);
            }
        }
    }
}

/// Completion thread, after the frame's fence: reads the timestamps back and hands the steps to the
/// callback.
///
/// # Safety
/// `td` is the slot of a frame that has completed and is not back in its pool.
pub(crate) unsafe fn deliver(td: &mut av1_frame_thread_data) {
    let steps = &mut td.step_time;
    if !std::mem::take(&mut steps.timed) {
        return;
    }
    let count = steps.steps.len();
    let Some(readback) = steps.readback.as_ref() else {
        return;
    };
    let Some(stamps) = readback.read((count + 1) * 8, |bytes| {
        bytes.as_chunks::<8>().0.iter().map(|chunk| u64::from_ne_bytes(*chunk)).collect::<Vec<u64>>()
    }) else {
        return;
    };
    let to_ms = if steps.frequency != 0 { 1000.0 / steps.frequency as f64 } else { 0.0 };
    for (record, pair) in steps.steps.iter_mut().zip(stamps.windows(2)) {
        record.gpu_ms = pair[1].saturating_sub(pair[0]) as f64 * to_ms;
    }
    let frame = sys::av1_intra_steps {
        frame_number: steps.frame_number,
        show_frame: steps.show_frame,
        intra_frame: steps.intra_frame,
        bit_depth: steps.bit_depth,
        tile_count: steps.tile_count,
        step_count: count as c_int,
        gpu_ms: stamps[count].saturating_sub(stamps[0]) as f64 * to_ms,
        steps: steps.steps.as_ptr(),
    };
    let target = config().as_ref().map(|config| (config.callback, config.context));
    if let Some((callback, context)) = target {
        // SAFETY: the callback the switch was given, with its context and a record valid during the call.
        unsafe { callback(context as *mut c_void, &frame) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kinds_follow_the_angle() {
        // V_PRED: 90 is straight, 87 zone 1, 93 zone 2; H_PRED: 180 straight, 183 zone 3.
        assert_eq!(kind(1, 0, false, false, false), sys::AV1_INTRA_KIND_STRAIGHT as usize);
        assert_eq!(kind(1, -1, false, false, false), sys::AV1_INTRA_KIND_ZONE1 as usize);
        assert_eq!(kind(1, 1, false, false, false), sys::AV1_INTRA_KIND_ZONE2 as usize);
        assert_eq!(kind(2, 0, false, false, false), sys::AV1_INTRA_KIND_STRAIGHT as usize);
        assert_eq!(kind(2, 1, false, false, false), sys::AV1_INTRA_KIND_ZONE3 as usize);
        assert_eq!(kind(4, 3, false, false, false), sys::AV1_INTRA_KIND_ZONE2 as usize);
        assert_eq!(kind(7, -3, false, false, false), sys::AV1_INTRA_KIND_ZONE3 as usize);
        assert_eq!(kind(8, 3, false, false, false), sys::AV1_INTRA_KIND_ZONE1 as usize);
        assert_eq!(kind(0, 0, true, false, false), sys::AV1_INTRA_KIND_FILTER as usize);
        assert_eq!(kind(13, 0, false, false, false), sys::AV1_INTRA_KIND_CFL as usize);
        assert_eq!(kind(10, 0, false, false, false), sys::AV1_INTRA_KIND_SMOOTH as usize);
        assert_eq!(kind(12, 0, false, false, false), sys::AV1_INTRA_KIND_PAETH as usize);
        assert_eq!(kind(0, 0, false, true, false), sys::AV1_INTRA_KIND_INTRABC as usize);
        assert_eq!(kind(9, 0, false, false, true), sys::AV1_INTRA_KIND_INTERINTRA as usize);
    }

    #[test]
    fn counts_grow_with_the_steps() {
        let mut tile: TileSteps = Vec::new();
        // SAFETY: the vector of this test.
        unsafe {
            count(std::ptr::from_mut(&mut tile).cast(), 3, 1, 2, 0);
            count(std::ptr::from_mut(&mut tile).cast(), 3, 1, 2, 5);
            count(std::ptr::null_mut(), 9, 0, 0, 0);
        }
        assert_eq!(tile.len(), 4);
        assert_eq!(tile[3].blocks[1][2], 2);
        assert_eq!(tile[3].filter_diagonals, 5);
    }
}
