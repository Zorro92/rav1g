// SPDX-License-Identifier: BSD-2-Clause
//
// This source code is subject to the terms of the BSD 2 Clause License and the Alliance for Open Media
// Patent License 1.0. If the BSD 2 Clause License was not distributed with this source code in the LICENSE
// file, you can obtain it at www.aomedia.org/license/software. If the Alliance for Open Media Patent
// License 1.0 was not distributed with this source code in the PATENTS file, you can obtain it at
// www.aomedia.org/license/patent.

//! The memory statistics of the test programs (`-memstats`; docs/memory.md): what the frames of a stream
//! really use of the buffers that are sized for the worst case, how many frame buffers are out of their
//! pool at once, and how many bytes the CPU writes for a frame into memory the GPU reads. A measurement:
//! nothing of the decoder depends on it, and with the switch off every hook returns after one load of an
//! atomic.
//!
//! The switch is process-wide like the others (`switches.rs`): 0 off, 1 the summary, 2 also a line for
//! every frame. The numbers are collected from the first frame that is set up with the switch on and
//! written as `MEMSTATS ` lines when the decoder has drained (or is released), which also starts them
//! over: one block a task of the test programs.

use std::ffi::c_int;
use std::sync::Mutex;
use std::sync::atomic::{AtomicI32, AtomicU64, Ordering};

use super::log::log;
use super::pool::{CHUNK_ITEMS, KINDS, Kind};
use super::thread::lock;
use super::types::{Av1Core, FrameBufferPoolSize, av1_frame_thread_data};
use crate::sys::{AV1_COMMON, MB_MODE_INFO};

static LEVEL: AtomicI32 = AtomicI32::new(0);

/// The switch: 0 off, 1 the summary of a drained decoder, 2 also a line a frame.
pub extern "C" fn av1_set_memory_stats(level: c_int) {
    LEVEL.store(level.clamp(0, 2), Ordering::Relaxed);
}

#[inline]
fn level() -> c_int {
    LEVEL.load(Ordering::Relaxed)
}

const SLOTS: usize = 4;
/// What a frame writes, counted per slot while the frame is in flight: mode info records, coefficient
/// values (with the padding of a transform block's count), transform block records, entries of the
/// block index list, bytes of the palette buffer.
const ITEMS: usize = 5;
const ITEM_NAMES: [&str; ITEMS] =
    ["mode info records", "coefficients", "transform block records", "block index entries", "palette bytes"];
const INDEX: usize = 3;
const PALETTE: usize = 4;

/// What the CPU writes for a frame, in bytes.
const CPU: usize = 8;
const CPU_NAMES: [&str; CPU] = [
    "mode info records",
    "mode info grid",
    "coefficients (at most)",
    "transform block records",
    "block index list",
    "cdef lists",
    "constants",
    "palette",
];

#[derive(Clone, Copy, Default)]
struct Series {
    count: u64,
    sum: u64,
    peak: u64,
}

impl Series {
    fn add(&mut self, value: u64) {
        self.count += 1;
        self.sum += value;
        self.peak = self.peak.max(value);
    }

    fn mean(&self) -> f64 {
        self.sum as f64 / self.count.max(1) as f64
    }
}

/// How often a small count was seen.
#[derive(Clone, Copy, Default)]
struct Histogram {
    seen: [u64; 16],
}

impl Histogram {
    fn add(&mut self, value: usize) {
        self.seen[value.min(15)] += 1;
    }

    fn text(&self) -> String {
        let total: u64 = self.seen.iter().sum();
        if total == 0 {
            return String::from("no frame");
        }
        let peak = self.seen.iter().rposition(|&count| count != 0).unwrap_or(0);
        let typical = (0..16).max_by_key(|&value| self.seen[value]).unwrap_or(0);
        let mean =
            self.seen.iter().enumerate().map(|(value, &count)| value as u64 * count).sum::<u64>() as f64 / total as f64;
        let each: Vec<String> = self
            .seen
            .iter()
            .enumerate()
            .filter(|&(_, &count)| count != 0)
            .map(|(value, count)| format!("{value}: {count}"))
            .collect();
        format!("peak {peak}, typical {typical}, mean {mean:.1} (frames by count: {})", each.join(", "))
    }
}

#[derive(Default)]
struct State {
    /// The frames that were set up, and those that were recorded.
    setups: u64,
    frames: u64,
    /// The largest frame, in pixels.
    size: (c_int, c_int),
    /// The chunks a frame holds of each pool when it is recorded.
    chunks: [Series; KINDS],
    items: [Series; ITEMS],
    pred_blocks: Series,
    warp_blocks: Series,
    cpu: [Series; CPU],
    cpu_total: Series,
    fb_in_use: Histogram,
    references: Histogram,
    screen_frames: u64,
    palette_frames: u64,
    palette_blocks: u64,
}

/// The statistics of a decoder.
pub(crate) struct MemStats {
    /// What the frame of a slot has written, from its setup until it gives its slot back.
    live: [[AtomicU64; ITEMS]; SLOTS],
    palette_blocks: [AtomicU64; SLOTS],
    /// The most of an item the frames in flight had at once, looked at when a tile ends.
    at_once: [AtomicU64; ITEMS],
    /// The records `gen_pred_blocks` writes for the frame that is being recorded: prediction blocks, and
    /// those of warped motion.
    pred: [AtomicU64; 2],
    state: Mutex<State>,
}

impl MemStats {
    pub(crate) fn new() -> MemStats {
        MemStats {
            live: [const { [const { AtomicU64::new(0) }; ITEMS] }; SLOTS],
            palette_blocks: [const { AtomicU64::new(0) }; SLOTS],
            at_once: [const { AtomicU64::new(0) }; ITEMS],
            pred: [const { AtomicU64::new(0) }; 2],
            state: Mutex::new(State::default()),
        }
    }

    fn add(&self, slot: usize, item: usize, count: u64) {
        self.live[slot][item].fetch_add(count, Ordering::Relaxed);
        let sum: u64 = self.live.iter().map(|live| live[item].load(Ordering::Relaxed)).sum();
        self.at_once[item].fetch_max(sum, Ordering::Relaxed);
    }

    fn clear(&self, slot: usize) {
        for item in &self.live[slot] {
            item.store(0, Ordering::Relaxed);
        }
        self.palette_blocks[slot].store(0, Ordering::Relaxed);
    }
}

fn slot_of(td: &av1_frame_thread_data) -> Option<usize> {
    usize::try_from(td.slot_index).ok().filter(|&slot| slot < SLOTS)
}

/// A frame is set up on the slot `td`, with its frame buffer and its references counted: the frame
/// buffers that are out of the pool, and the frames the eight reference slots name.
///
/// # Safety
/// `dec` is the decoder, in the setup of the frame of `cm` on the slot `td`.
pub(crate) unsafe fn frame_setup(dec: *mut Av1Core, cm: &AV1_COMMON, td: &av1_frame_thread_data) {
    if level() == 0 {
        return;
    }
    // SAFETY: the caller's contract; the reference counts are read under the lock of the pool.
    unsafe {
        let stats = &(*dec).mem_stats;
        let Some(slot) = slot_of(td) else {
            return;
        };
        stats.clear(slot);
        let in_use = {
            let _pool = (*dec).fb_pool.lock();
            (0..FrameBufferPoolSize).filter(|&buffer| (*dec).fb_pool_src[buffer].ref_cnt != 0).count()
        };
        let mut buffers = [std::ptr::null_mut(); 8];
        let mut distinct = 0;
        for reference in cm.ref_frame_map {
            let Some(reference) = reference.as_ref() else {
                continue;
            };
            let buffer = reference.buf.hw_buffer;
            if !buffer.is_null() && !buffers[..distinct].contains(&buffer) {
                buffers[distinct] = buffer;
                distinct += 1;
            }
        }
        let mut state = lock(&stats.state);
        if state.setups == 0 {
            // The first frame of the statistics: what the pools held before it does not count.
            if let Some(pools) = (*dec).pools.as_deref() {
                pools.take_stat_peak();
            }
            for item in &stats.at_once {
                item.store(0, Ordering::Relaxed);
            }
        }
        state.setups += 1;
        state.fb_in_use.add(in_use);
        state.references.add(distinct);
        state.screen_frames += u64::from(cm.allow_screen_content_tools != 0);
    }
}

/// A tile of the frame on the slot `td` is parsed: what it wrote.
///
/// # Safety
/// `dec` is the decoder and `td` a slot of it.
pub(crate) unsafe fn tile_parsed(
    dec: *mut Av1Core,
    td: *const av1_frame_thread_data,
    mode_info: u64,
    coefs: u64,
    txb: u64,
    indexes: u64,
) {
    if level() == 0 {
        return;
    }
    // SAFETY: the caller's contract.
    unsafe {
        let stats = &(*dec).mem_stats;
        let Some(slot) = slot_of(&*td) else {
            return;
        };
        stats.add(slot, Kind::ModeInfo as usize, mode_info);
        stats.add(slot, Kind::Coefs as usize, coefs);
        stats.add(slot, Kind::Txb as usize, txb);
        stats.add(slot, INDEX, indexes);
    }
}

/// A palette block's plane of `bytes` bytes was written into the palette buffer of the slot `td`.
///
/// # Safety
/// `dec` is the decoder and `td` a slot of it.
pub(crate) unsafe fn palette_block(dec: *mut Av1Core, td: *const av1_frame_thread_data, bytes: usize) {
    if level() == 0 || dec.is_null() {
        return;
    }
    // SAFETY: the caller's contract.
    unsafe {
        let stats = &(*dec).mem_stats;
        if let Some(slot) = slot_of(&*td) {
            stats.add(slot, PALETTE, bytes as u64);
            stats.palette_blocks[slot].fetch_add(1, Ordering::Relaxed);
        }
    }
}

/// `gen_pred_blocks` of the frame that is being recorded writes this many records.
///
/// # Safety
/// `dec` is the decoder.
pub(crate) unsafe fn pred_blocks(dec: *mut Av1Core, blocks: c_int, warp: c_int) {
    if level() == 0 {
        return;
    }
    // SAFETY: the caller's contract.
    unsafe {
        let stats = &(*dec).mem_stats;
        stats.pred[0].store(blocks.max(0) as u64, Ordering::Relaxed);
        stats.pred[1].store(warp.max(0) as u64, Ordering::Relaxed);
    }
}

/// The frame of `cm` on the current slot is recorded, up to its submission.
///
/// # Safety
/// `dec` is the decoder on the thread that records the frame, behind its last stage.
pub(crate) unsafe fn frame_recorded(dec: *mut Av1Core, cm: &AV1_COMMON) {
    let level = level();
    if level == 0 {
        return;
    }
    // SAFETY: the caller's contract.
    unsafe {
        let stats = &(*dec).mem_stats;
        let Some(td) = (*dec).curr_frame_data.as_ref() else {
            return;
        };
        let Some(slot) = slot_of(td).filter(|_| td.output_only == 0) else {
            return;
        };
        let items: [u64; ITEMS] = std::array::from_fn(|item| stats.live[slot][item].load(Ordering::Relaxed));
        let palette_blocks = stats.palette_blocks[slot].load(Ordering::Relaxed);
        let held = (*dec).pools.as_deref().map_or([0; KINDS], |pools| pools.held(slot));
        let pred = [stats.pred[0].load(Ordering::Relaxed), stats.pred[1].load(Ordering::Relaxed)];
        let cdef = if td.do_cdef != 0 {
            let (nvfb, nhfb) = ((cm.mi_rows + 15) / 16, (cm.mi_cols + 15) / 16);
            (nvfb * nhfb + (nvfb * 16).min(cm.mi_rows / 2) * (nhfb * 16).min(cm.mi_cols / 2)) as u64 * 4
        } else {
            0
        };
        let cpu: [u64; CPU] = [
            items[0] * size_of::<MB_MODE_INFO>() as u64,
            (cm.mi_cols * cm.mi_rows).max(0) as u64 * size_of::<*mut MB_MODE_INFO>() as u64,
            items[1] * 4,
            items[2] * 16,
            items[INDEX] * 4,
            cdef,
            td.command_buffer.cb_ptr as u64,
            items[PALETTE],
        ];
        let cpu_total: u64 = cpu.iter().sum();
        let mut state = lock(&stats.state);
        state.frames += 1;
        if cm.width * cm.height > state.size.0 * state.size.1 {
            state.size = (cm.width, cm.height);
        }
        for kind in 0..KINDS {
            state.chunks[kind].add(held[kind] as u64);
        }
        for item in 0..ITEMS {
            state.items[item].add(items[item]);
        }
        state.pred_blocks.add(pred[0]);
        state.warp_blocks.add(pred[1]);
        for part in 0..CPU {
            state.cpu[part].add(cpu[part]);
        }
        state.cpu_total.add(cpu_total);
        state.palette_frames += u64::from(palette_blocks != 0);
        state.palette_blocks += palette_blocks;
        let number = state.frames;
        drop(state);
        if level >= 2 {
            log(
                false,
                &format!(
                    "MEMSTATS frame {number}: {}x{} tiles {} slot {slot}; chunks held: mode info {}, coefficients \
                     {}, transform block records {}; written: mode info records {}, coefficients {}, transform \
                     block records {}, block index entries {}, prediction blocks {}, warp blocks {}, palette \
                     blocks {palette_blocks} of {} bytes; cpu bytes {cpu_total}: mode info {}, grid {}, \
                     coefficients at most {}, transform block records {}, block index list {}, cdef lists {}, \
                     constants {}, palette {}",
                    cm.width,
                    cm.height,
                    td.tile_count,
                    held[0],
                    held[1],
                    held[2],
                    items[0],
                    items[1],
                    items[2],
                    items[INDEX],
                    pred[0],
                    pred[1],
                    items[PALETTE],
                    cpu[0],
                    cpu[1],
                    cpu[2],
                    cpu[3],
                    cpu[4],
                    cpu[5],
                    cpu[6],
                    cpu[7],
                ),
            );
        }
    }
}

/// The frame of the slot `td` has given back what it held (its GPU work is done, or it failed).
///
/// # Safety
/// `dec` is the decoder and `td` a slot of it.
pub(crate) unsafe fn frame_released(dec: *mut Av1Core, td: *const av1_frame_thread_data) {
    if level() == 0 {
        return;
    }
    // SAFETY: the caller's contract.
    unsafe {
        if let Some(slot) = slot_of(&*td).filter(|_| (*td).output_only == 0) {
            (*dec).mem_stats.clear(slot);
        }
    }
}

fn mib(bytes: f64) -> f64 {
    bytes / (1024.0 * 1024.0)
}

fn percent(part: f64, whole: f64) -> f64 {
    if whole > 0.0 { 100.0 * part / whole } else { 0.0 }
}

/// The summary of the frames since the last one, as `MEMSTATS ` lines, and the statistics start over.
/// Nothing when no frame was counted.
///
/// # Safety
/// `dec` is a decoder with no frame in flight (drained, or being released with its buffers still there).
pub(crate) unsafe fn summary(dec: *mut Av1Core) {
    // SAFETY: the caller's contract.
    unsafe {
        let stats = &(*dec).mem_stats;
        let state = std::mem::take(&mut *lock(&stats.state));
        if state.setups == 0 {
            return;
        }
        let at_once: [u64; ITEMS] = std::array::from_fn(|item| stats.at_once[item].swap(0, Ordering::Relaxed));
        let pool_peak = (*dec).pools.as_deref().map_or([0; KINDS], |pools| pools.take_stat_peak());
        let say = |text: String| log(false, &format!("MEMSTATS {text}"));
        let size = |buffer: *mut super::types::GpuBufferObject| buffer.as_ref().map_or(0, |buffer| buffer.size);
        let slots = usize::try_from((*dec).frame_slots).unwrap_or(0).min(SLOTS);
        say(format!(
            "begin: {} frames recorded ({} set up), largest {}x{}; decoder {}x{} {}-bit, {} slots, {} frames in \
             parse; per frame = of the frames recorded, at once = of all frames in flight together",
            state.frames,
            state.setups,
            state.size.0,
            state.size.1,
            (*dec).max_width,
            (*dec).max_height,
            (*dec).max_bitdepth,
            slots,
            (*dec).frames_in_parse
        ));
        // The pools.
        let pool_buffers = [(*dec).mode_info_pool, (*dec).idct_coefs, (*dec).idct_blocks_pool];
        let pool_names = ["mode_info_pool", "idct_coefs", "idct_blocks_unordered"];
        for kind in 0..KINDS {
            let total = (*dec).pool_total[kind];
            let bytes = size(pool_buffers[kind]) as f64;
            let chunk_mib = mib(bytes) / total.max(1) as f64;
            let capacity = (total * CHUNK_ITEMS[kind]) as f64;
            say(format!(
                "pool {}: {total} chunks of {} {}, {:.1} MiB; chunks in use at once peak {} ({:.1} %, {:.1} MiB); \
                 chunks of a frame peak {} mean {:.1}; {} of a frame peak {} ({:.1} % of the pool) mean {:.0}, at \
                 once peak {} ({:.1} %); fill of the chunks held {:.1} %",
                pool_names[kind],
                CHUNK_ITEMS[kind],
                ITEM_NAMES[kind],
                mib(bytes),
                pool_peak[kind],
                percent(pool_peak[kind] as f64, total as f64),
                pool_peak[kind] as f64 * chunk_mib,
                state.chunks[kind].peak,
                state.chunks[kind].mean(),
                ITEM_NAMES[kind],
                state.items[kind].peak,
                percent(state.items[kind].peak as f64, capacity),
                state.items[kind].mean(),
                at_once[kind],
                percent(at_once[kind] as f64, capacity),
                percent(state.items[kind].sum as f64, state.chunks[kind].sum as f64 * CHUNK_ITEMS[kind] as f64),
            ));
        }
        // The buffers of the stages and of a slot that hold a worst case.
        let buffer = |name: &str, bytes: usize, record: usize, per_slot: bool, series: &Series| {
            let records = (bytes / record) as f64;
            say(format!(
                "buffer {name}: {:.1} MiB{}, {records:.0} records of {record} bytes; of a frame peak {} ({:.1} %, \
                 {:.2} MiB) mean {:.0} ({:.1} %)",
                mib(bytes as f64),
                if per_slot { " a slot" } else { "" },
                series.peak,
                percent(series.peak as f64, records),
                mib((series.peak * record as u64) as f64),
                series.mean(),
                percent(series.mean(), records),
            ));
        };
        buffer(
            "gen_mi_block_indexes",
            size((*dec).frame_thread_data[0].gen_mi_block_indexes),
            4,
            true,
            &state.items[INDEX],
        );
        buffer("prediction_blocks", size((*dec).prediction_blocks), 16, false, &state.pred_blocks);
        buffer("prediction_blocks_warp", size((*dec).prediction_blocks_warp), 48, false, &state.warp_blocks);
        buffer("idct_blocks", size((*dec).idct_blocks), 16, false, &state.items[Kind::Txb as usize]);
        // The frame buffers.
        let fb_total = (0..FrameBufferPoolSize).filter(|&fb| (*dec).fb_pool_src[fb].size != 0).count();
        say(format!(
            "frame buffers: {fb_total} in the pool and 2 back buffers, {:.1} MiB each; out of the pool at the setup \
             of a frame, that frame's included: {}",
            mib((*dec).fb_size as f64),
            state.fb_in_use.text()
        ));
        say(format!(
            "references: frames the 8 reference slots name at the setup of a frame: {}",
            state.references.text()
        ));
        // The palette buffers.
        let created = (0..slots).filter(|&slot| !(*dec).frame_thread_data[slot].palette_buffer.is_null()).count();
        say(format!(
            "palette: {created} of {slots} buffers created, {:.1} MiB each; frames that allow screen content tools \
             {}, frames with palette blocks {}, palette blocks {}; bytes written of a frame peak {} mean {:.0}, at \
             once peak {}",
            mib((((*dec).palette_size + 255) & !255) as f64),
            state.screen_frames,
            state.palette_frames,
            state.palette_blocks,
            state.items[PALETTE].peak,
            state.items[PALETTE].mean(),
            at_once[PALETTE],
        ));
        // What the CPU writes.
        let parts: Vec<String> = (0..CPU)
            .map(|part| format!("{} {:.0} / {}", CPU_NAMES[part], state.cpu[part].mean(), state.cpu[part].peak))
            .collect();
        say(format!(
            "cpu bytes written for a frame into memory the GPU reads, mean / peak: all {:.0} / {} ({:.2} / {:.2} \
             MiB); {}; not counted: the counters of the block lists, loop restoration units, film grain offsets, \
             clears (see the GPUTIME lines)",
            state.cpu_total.mean(),
            state.cpu_total.peak,
            mib(state.cpu_total.mean()),
            mib(state.cpu_total.peak as f64),
            parts.join(", ")
        ));
        say(String::from("end"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_histogram_says_peak_and_typical() {
        let mut seen = Histogram::default();
        for value in [8, 8, 8, 9, 3] {
            seen.add(value);
        }
        assert_eq!(seen.text(), "peak 9, typical 8, mean 7.2 (frames by count: 3: 1, 8: 3, 9: 1)");
        assert_eq!(Histogram::default().text(), "no frame");
    }

    #[test]
    fn the_items_of_the_frames_in_flight_add_up() {
        let stats = MemStats::new();
        stats.add(0, 1, 10);
        stats.add(1, 1, 5);
        stats.clear(0);
        stats.add(0, 1, 2);
        assert_eq!(stats.at_once[1].load(Ordering::Relaxed), 15);
        assert_eq!(stats.live[0][1].load(Ordering::Relaxed), 2);
    }
}
