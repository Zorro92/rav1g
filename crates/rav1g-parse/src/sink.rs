// SPDX-License-Identifier: BSD-2-Clause
//
// This source code is subject to the terms of the BSD 2 Clause License and the Alliance for Open Media
// Patent License 1.0. If the BSD 2 Clause License was not distributed with this source code in the LICENSE
// file, you can obtain it at www.aomedia.org/license/software. If the Alliance for Open Media Patent
// License 1.0 was not distributed with this source code in the PATENTS file, you can obtain it at
// www.aomedia.org/license/patent.

//! The sink of rav1d's parse pass that writes what the GPU stages read, in the layouts the entropy
//! decoder of libaom writes them in today:
//!
//! - a mode info record per block (`record`), in the tile's part of the mode info pool, and its address
//!   in the grid at every 4x4 unit of the block inside the frame (`set_offsets` of
//!   `av1/decoder/decodeframe.c`);
//! - a record per coded transform block and its coefficients, dequantised and in scan order
//!   (`av1_read_coeffs_txb` of `av1/decoder/decodetxb.c`);
//! - the block lists of the prediction stages, at the end of a tile, from the records in the order they
//!   were read: `av1_mi_push_block`, the host's function, called as the entropy decoder calls it
//!   (`decode_tile2`).
//!
//! The memory is the host's ([`SinkHost`]): for the D3D12 host the mapped buffers of a pipeline slot, for
//! `tools/rav1g-parse-check` plain memory.
//!
//! Two of those buffers are write-combined memory with the D3D12 host (the coefficients and the
//! transform block records: upload heaps, which the CPU must not read and should write in order). The
//! sink writes them as the entropy decoder of libaom does: each record once, in order, and of the
//! coefficients the ones that are not zero, in rising scan order. With [`SinkOptions::stage`] both go
//! through memory of the sink first and reach the buffers in runs, whole and in order.
//!
//! Until the entropy decoder of libaom leaves the tree (docs/phase2-rav1d.md, step 6), the block lists are
//! computed by the code that takes its block context from that decoder's structs. So the sink keeps such
//! structs of its own, cleared, and sets the fields that code reads: an `AV1Decoder` for the frame (frame
//! size in 4x4 units, grid, superblock size, the host's decoder) and a `MACROBLOCKD` per tile (the block's
//! place in the grid, what is available around it, the tile).

use std::alloc::{Layout, alloc_zeroed, handle_alloc_error};
use std::cell::UnsafeCell;
use std::ffi::{c_int, c_void};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use rav1d::include::dav1d::headers::Rav1dSequenceHeader;
use rav1d::include::dav1d::headers::{Rav1dFrameHeader, Rav1dPixelLayout};
use rav1d::parse::{
    ParseBlock, ParseFrame, ParsePalette, ParseShowExisting, ParseSink, ParseSuperblock, ParseTile, ParseTxb,
};
use rav1g_sys::parser::{
    AV1_COMMON, AV1Decoder, MACROBLOCKD, MB_MODE_INFO, TileInfo, av1_tile_data, tx_block_info_gpu,
};

use crate::record::{self, BLOCK_SIZE_OF, MI_SIZE_HIGH_LOG2, MI_SIZE_WIDE_LOG2, RecordFrame};

unsafe extern "C" {
    /// `dx/av1_core.h`: counts the prediction blocks of the block `xd` is at into the lists of its tile.
    /// The host's function (`crates/rav1g/src/host/blocks.rs`).
    fn av1_mi_push_block(pbi: *mut AV1Decoder, cm: *mut AV1_COMMON, xd: *mut MACROBLOCKD);
}

/// `TX_SIZES_ALL`: the entry of a tile's transform block counts for the 4x4 blocks of a lossless segment.
const TX_SIZES_ALL: usize = 19;
/// `WHT_WHT` of rav1d: the transform of a lossless block, which libaom knows as `DCT_DCT` of a lossless
/// segment.
const WHT_WHT: u8 = 16;
/// The bits of a transform block record that say which one-dimensional transforms a transform type is
/// made of (`tx_types_flags` of `av1_read_coeffs_txb`), by `TX_TYPE`.
const TX_TYPE_FLAGS: [u32; 16] = [
    0, 0x08_0000, 0x02_0000, 0x0A_0000, 0x09_0000, 0x02_8000, 0x0B_8000, 0x0A_8000, 0x0B_0000, 0x14_0000, 0x04_0000,
    0x10_0000, 0x0C_0000, 0x12_0000, 0x0D_0000, 0x12_8000,
];
/// The scan of a transform type (`av1_idct_scans_lut` of `av1_read_coeffs_txb`): 0 the default one, 1 and
/// 2 the scans by row and by column.
const TX_TYPE_SCAN: [u32; 16] = [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 2, 1, 2, 1, 2];

/// The largest number of coefficients a transform block has (32x32 coded).
const MAX_COEFS: usize = 1024;
/// The coefficients a tile stages before they go to their buffer ([`SinkOptions::stage`]): 32 KiB, which
/// stays in the first-level cache next to what the parse reads.
const STAGE_COEFS: usize = 8 * MAX_COEFS;
/// The transform block records a tile stages: 8 KiB.
const STAGE_BLOCKS: usize = 512;

/// How the sink works. All off is the sink as it writes by default.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SinkOptions {
    /// The coefficients and the transform block records of a tile are collected in memory of the sink
    /// and copied to their buffers in runs: every word of a run is written, in order, by one copy. The
    /// buffers hold the same bytes afterwards (a coefficient that is zero is then written as zero, where
    /// it is otherwise left as the zero the buffer has).
    pub stage: bool,
    /// The time inside the calls of a tile (block, transform block, palette, superblock) is measured
    /// and given to the host ([`SinkTimer::Calls`]), when the host has a clock. Two readings of the
    /// processor's time stamp counter a call.
    pub time_calls: bool,
}

/// What the sink measures for a host with a clock ([`SinkHost::add_time`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SinkTimer {
    /// The calls of a tile: the mode info record and the grid of a block, the record of a transform
    /// block, the palette, the superblock; with [`SinkOptions::stage`] the copies of the staged runs too.
    /// Only with [`SinkOptions::time_calls`]. The parse that makes the calls is not in it, and neither
    /// are the coefficients the parse writes itself.
    Calls,
    /// The copies of the staged runs into their buffers ([`SinkOptions::stage`]).
    Staged,
    /// The block lists at the end of a tile.
    Lists,
}

/// The processor's time stamp counter, or 0 where there is none to read.
#[inline]
fn time_stamp() -> u64 {
    #[cfg(target_arch = "x86_64")]
    {
        // SAFETY: the instruction is part of every x86-64 processor and reads a counter.
        #[allow(unused_unsafe, reason = "safe to call from some toolchains on")]
        unsafe {
            core::arch::x86_64::_rdtsc()
        }
    }
    #[cfg(not(target_arch = "x86_64"))]
    {
        0
    }
}

/// The memory of a frame's parse that all its tiles share.
pub struct FrameMemory {
    /// The grid: the address of the block's record at every 4x4 unit, `mi_stride` a row, all null.
    pub mi_grid: *mut *mut MB_MODE_INFO,
    pub mi_stride: c_int,
}

/// The memory of a tile's parse.
pub struct TileMemory {
    /// The tile's cursors into the buffers of the frame, set up as `av1_setup_macroblockd` sets them up.
    pub tile_data: *mut av1_tile_data,
    /// The tile's part of the mode info pool and the records it has room for.
    pub mi_pool: *mut MB_MODE_INFO,
    pub mi_count_max: c_int,
    /// The tile's part of the coefficient buffer has the worst-case layout: it is as large as the tile
    /// can need, and the tile never goes on in a second part (`ThreadData::ext_idct_buffer`).
    pub worst_case_coefs: bool,
}

/// The second part of a tile's mode info ([`SinkHost::second_mode_info`]): its cursors, and the records,
/// with room for as many as the first part.
pub struct SecondModeInfo {
    pub tile_data: *mut av1_tile_data,
    pub mi_pool: *mut MB_MODE_INFO,
}

/// What gives the sink its memory: the host of the decoder.
pub trait SinkHost: Send + Sync {
    /// The host's decoder: `gpu_decoder` of the `AV1Decoder` that `av1_mi_push_block` is called with.
    fn gpu_decoder(&self) -> *mut c_void;

    /// A sequence header (`ParseSink::sequence`). `Err` refuses the sequence.
    #[expect(clippy::result_unit_err, reason = "the error of ParseSink::sequence, which has no more to say")]
    fn sequence(&self, _seq_hdr: &Rav1dSequenceHeader, _new: bool) -> Result<(), ()> {
        Ok(())
    }

    /// A frame header that shows the frame of a reference slot (`ParseSink::show_existing`).
    #[expect(clippy::result_unit_err, reason = "the error of ParseSink::show_existing, which has no more to say")]
    fn show_existing(&self, _show: &ParseShowExisting<'_>) -> Result<(), ()> {
        Ok(())
    }

    /// The clock of [`Self::tile_time`], or 0 when nothing is timed.
    fn ticks(&self) -> i64 {
        0
    }

    /// A superblock row of a tile was parsed since `start`, with the sink's part of it.
    fn tile_time(&self, _start: i64) {}

    /// A part of the sink's work took `ticks` of the clock of [`Self::ticks`]. On a tile's thread.
    fn add_time(&self, _timer: SinkTimer, _ticks: i64) {}

    /// A frame's parse starts (frames come one at a time). `Err` fails the frame.
    #[expect(clippy::result_unit_err, reason = "the error of ParseSink::frame_begin, which has no more to say")]
    fn frame_begin(&self, frame: &ParseFrame<'_>) -> Result<FrameMemory, ()>;

    /// A tile's parse starts, on the thread that parses it.
    fn tile_begin(&self, tile: &ParseTile) -> TileMemory;

    /// The tile's part of the mode info pool is full: where its records go on (`av1_setup_sec_data`).
    /// `None` when there is no second part: the frame fails.
    fn second_mode_info(&self, tile: usize, tile_data: *mut av1_tile_data) -> Option<SecondModeInfo>;

    /// The tile's part of the coefficient buffer is used up to its mark (`dq_buffer_max`): the host moves
    /// the tile's cursor to where its coefficients go on (`av1_setup_ext_coef_buffer`).
    fn second_coefs(&self, tile: usize, tile_data: *mut av1_tile_data);

    /// A tile has no room left for its blocks (`mode_info`) or for its coefficients, in two parts: the
    /// frame fails. On the tile's thread.
    fn no_room(&self, _tile: usize, _mode_info: bool) {}

    /// The colour index map of a palette block's plane (`av1_intra_palette`).
    fn palette(&self, tile: usize, palette: &ParsePalette<'_>);

    /// A superblock is parsed: its CDEF indexes and loop restoration units.
    fn superblock(&self, tile: usize, superblock: &ParseSuperblock<'_>);

    /// A tile is parsed and its block lists are counted.
    fn tile_end(&self, tile: usize);

    /// The frame's parse has ended. Returns whether the frame stands (`ParseSink::frame_end`).
    fn frame_end(&self, ok: bool) -> bool;
}

/// What the sink keeps of the frame in parse.
struct FrameState {
    hdr: Rav1dFrameHeader,
    mi_cols: c_int,
    mi_rows: c_int,
    mi_stride: c_int,
    mi_grid: *mut *mut MB_MODE_INFO,
    has_chroma: bool,
    ss_x: c_int,
    ss_y: c_int,
    /// The superblocks are 128x128.
    sb128: bool,
    /// The frame codes CDEF indexes (`read_cdef` of libaom: not in a frame that is lossless throughout
    /// or has intra block copy).
    cdef_coded: bool,
}

/// What the sink keeps of a tile in parse. One thread at a time works on a tile.
struct TileState {
    /// The block context `av1_mi_push_block` reads, cleared, with the tile's constants set.
    xd: Box<MACROBLOCKD>,
    memory: TileMemory,
    mi_count: c_int,
    /// The second part of the tile's mode info, when the first is full, and the records in it.
    second: Option<SecondModeInfo>,
    mi_count2: c_int,
    /// The tile's coefficients have their last part: the worst-case layout, or the second part.
    last_coefs: bool,
    /// The segment of the tile's last block is lossless.
    lossless: bool,
    /// The tile ran out of room: its frame fails.
    failed: bool,
    /// [`SinkOptions::stage`]: what the tile has collected for its buffers.
    stage: Option<Stage>,
    /// [`SinkOptions::time_calls`]: the time stamps the calls of the tile took, and the clock and the
    /// time stamp at the tile's begin, which give the length of a time stamp.
    call_stamps: u64,
    begin_ticks: i64,
    begin_stamp: u64,
}

/// The coefficients and the transform block records a tile has collected ([`SinkOptions::stage`]): a run
/// of each, which lies in the buffer from `coef_dst` and `block_dst` on.
struct Stage {
    /// Zero behind the run, as the buffer is where nothing was written.
    coefs: Box<[i32; STAGE_COEFS]>,
    coef_dst: *mut i32,
    coef_count: usize,
    blocks: Box<[tx_block_info_gpu; STAGE_BLOCKS]>,
    block_dst: *mut tx_block_info_gpu,
    block_count: usize,
}

impl Stage {
    fn new() -> Stage {
        Stage {
            coefs: zeroed_box(),
            coef_dst: std::ptr::null_mut(),
            coef_count: 0,
            blocks: zeroed_box(),
            block_dst: std::ptr::null_mut(),
            block_count: 0,
        }
    }

    fn is_empty(&self) -> bool {
        self.coef_count == 0 && self.block_count == 0
    }

    /// The runs go to their buffers.
    ///
    /// # Safety
    /// The buffers the runs were collected for are still the tile's.
    unsafe fn flush(&mut self) {
        // SAFETY: a run lies inside the tile's part of its buffer (checked when it was collected), and
        // the stage is memory of its own.
        unsafe {
            if self.coef_count != 0 {
                std::ptr::copy_nonoverlapping(self.coefs.as_ptr(), self.coef_dst, self.coef_count);
            }
            if self.block_count != 0 {
                std::ptr::copy_nonoverlapping(self.blocks.as_ptr(), self.block_dst, self.block_count);
            }
        }
        self.forget();
    }

    /// The runs are dropped (a tile that begins anew, behind a frame that failed).
    fn forget(&mut self) {
        self.coefs[..self.coef_count].fill(0);
        self.coef_count = 0;
        self.block_count = 0;
    }
}

/// A cleared struct of the entropy decoder on the heap.
fn zeroed_box<T>() -> Box<T> {
    let layout = Layout::new::<T>();
    // SAFETY: the structs this is used for are C structs of numbers and pointers, for which all zero
    // bytes are a value; the allocation has the layout of `T`.
    unsafe {
        let memory = alloc_zeroed(layout);
        if memory.is_null() {
            handle_alloc_error(layout);
        }
        Box::from_raw(memory.cast::<T>())
    }
}

/// The sink.
pub struct Sink<H: SinkHost> {
    host: H,
    /// Written by `frame_begin`, while no tile of the sink is in parse; read by the tiles.
    frame: UnsafeCell<Option<FrameState>>,
    /// The frame for `av1_mi_push_block`; written like `frame`.
    pbi: UnsafeCell<Box<AV1Decoder>>,
    /// The tiles of the frame; the vector is written by `frame_begin`, an element by its tile's thread.
    tiles: UnsafeCell<Vec<UnsafeCell<Option<TileState>>>>,
    /// Nanoseconds the tiles spent counting their block lists (`av1_mi_push_block`), for a timing run.
    block_list_nanos: AtomicU64,
    options: SinkOptions,
}

// SAFETY: the calls of a `ParseSink` come as its contract says: `frame_begin` and `frame_end` while no
// tile of the frame is in parse, the calls of a tile from one thread at a time. The fields are written
// under that contract (see their comments); the raw pointers in them are the host's memory, which the host
// gives for the frame.
unsafe impl<H: SinkHost> Send for Sink<H> {}
// SAFETY: as above.
unsafe impl<H: SinkHost> Sync for Sink<H> {}

impl<H: SinkHost> Sink<H> {
    /// Before a superblock of a tile: when the tile's part of the coefficient buffer is used up to its
    /// mark, the tile goes on in its second part (`decode_tile2` of `av1/decoder/decodeframe.c`).
    ///
    /// # Safety
    /// The tile's thread, inside the frame; `state.memory.tile_data` is the tile's.
    unsafe fn coefs_go_on(host: &H, tile: usize, state: &mut TileState) {
        // SAFETY: the caller's contract.
        unsafe {
            let td = state.memory.tile_data;
            if !state.last_coefs && (*td).dq_buffer_ptr >= (*td).dq_buffer_max {
                host.second_coefs(tile, td);
                state.last_coefs = true;
            }
        }
    }

    pub fn new(host: H) -> Self {
        Self::with_options(host, SinkOptions::default())
    }

    pub fn with_options(host: H, options: SinkOptions) -> Self {
        Self {
            host,
            frame: UnsafeCell::new(None),
            pbi: UnsafeCell::new(zeroed_box()),
            tiles: UnsafeCell::new(Vec::new()),
            block_list_nanos: AtomicU64::new(0),
            options,
        }
    }

    pub fn options(&self) -> SinkOptions {
        self.options
    }

    /// The staged runs of a tile go to their buffers ([`SinkOptions::stage`]).
    ///
    /// # Safety
    /// The tile's thread, inside the frame.
    unsafe fn flush_stage(host: &H, state: &mut TileState) {
        if let Some(stage) = &mut state.stage
            && !stage.is_empty()
        {
            let start = host.ticks();
            // SAFETY: the caller's contract: the runs were collected for the tile's buffers in this
            // frame.
            unsafe { stage.flush() };
            if start != 0 {
                host.add_time(SinkTimer::Staged, host.ticks() - start);
            }
        }
    }

    /// The start of a call of a tile that is timed ([`SinkOptions::time_calls`]), or 0.
    #[inline]
    fn call_begin(&self) -> u64 {
        if self.options.time_calls { time_stamp() } else { 0 }
    }

    /// The end of the call that began at `begin`.
    ///
    /// # Safety
    /// As [`Self::tile`].
    #[inline]
    unsafe fn call_end(&self, tile: usize, begin: u64) {
        if begin != 0 {
            // SAFETY: the caller's contract.
            if let Some(state) = unsafe { self.tile(tile) } {
                state.call_stamps += time_stamp().wrapping_sub(begin);
            }
        }
    }

    /// Nanoseconds the tiles have spent counting their block lists since the sink was made.
    pub fn block_list_nanos(&self) -> u64 {
        self.block_list_nanos.load(Ordering::Relaxed)
    }

    pub fn host(&self) -> &H {
        &self.host
    }

    /// The frame in parse.
    ///
    /// # Safety
    /// Between the `frame_begin` and the `frame_end` of a frame.
    unsafe fn frame(&self) -> &FrameState {
        // SAFETY: the caller's contract: the frame was set by `frame_begin` and is not written until
        // `frame_end`.
        unsafe { (*self.frame.get()).as_ref().expect("a frame is in parse") }
    }

    /// The state of a tile of the frame in parse.
    ///
    /// # Safety
    /// As [`Self::frame`], from the thread that parses the tile.
    #[expect(clippy::mut_from_ref, reason = "a tile's state is its thread's, by the contract of ParseSink")]
    unsafe fn tile(&self, tile: usize) -> &mut Option<TileState> {
        // SAFETY: the caller's contract: the vector is not written while tiles are in parse, and the
        // element is this thread's.
        unsafe { &mut *(&*self.tiles.get())[tile].get() }
    }
}

/// The calls of a tile, without their timing.
impl<H: SinkHost> Sink<H> {
    fn block_call(&self, _frame: usize, tile: usize, block: &ParseBlock<'_>) {
        // SAFETY: a call of the tile's thread between the frame's begin and end; the record is the next
        // one of the tile's part of the pool, and the grid is written inside the tile.
        unsafe {
            let frame = self.frame();
            let state = self.tile(tile).as_mut().expect("the tile has begun");
            if state.failed {
                return;
            }
            // The tile's part of the pool, and when it is full its second part (`decode_mbmi_block`,
            // `set_offsets`).
            if state.mi_count >= state.memory.mi_count_max && state.second.is_none() {
                state.second = self.host.second_mode_info(tile, state.memory.tile_data);
                if state.second.is_none() {
                    state.failed = true;
                    self.host.no_room(tile, true);
                    return;
                }
            }
            let mi = match &state.second {
                Some(second) => {
                    if state.mi_count2 >= state.memory.mi_count_max {
                        state.failed = true;
                        self.host.no_room(tile, true);
                        return;
                    }
                    state.mi_count2 += 1;
                    second.mi_pool.add(state.mi_count2 as usize - 1)
                }
                None => {
                    state.mi_count += 1;
                    state.memory.mi_pool.add(state.mi_count as usize - 1)
                }
            };
            mi.write_bytes(0, 1);
            let lossless = frame.hdr.segmentation.lossless[block.b.seg_id.get()];
            state.lossless = lossless;
            let record_frame = RecordFrame {
                hdr: &frame.hdr,
                mi_cols: frame.mi_cols,
                mi_rows: frame.mi_rows,
                has_chroma: frame.has_chroma,
            };
            record::fill(&mut *mi, &record_frame, block, lossless);

            // The grid: the units of the block that are inside the frame.
            let bsize = BLOCK_SIZE_OF[block.bs as usize] as usize;
            let x_mis = (1 << MI_SIZE_WIDE_LOG2[bsize]).min(frame.mi_cols - block.x4) as usize;
            let y_mis = (1 << MI_SIZE_HIGH_LOG2[bsize]).min(frame.mi_rows - block.y4) as usize;
            let first = frame.mi_grid.add((block.y4 * frame.mi_stride + block.x4) as usize);
            for y in 0..y_mis {
                let row = first.add(y * frame.mi_stride as usize);
                for x in 0..x_mis {
                    row.add(x).write(mi);
                }
            }
        }
    }

    fn txb_call(&self, _frame: usize, tile: usize, txb: &ParseTxb) -> *mut i32 {
        // SAFETY: a call of the tile's thread between the frame's begin and end; the record and the
        // coefficients are inside the tile's parts of their buffers (checked against their ends).
        unsafe {
            let state = self.tile(tile).as_mut().expect("the tile has begun");
            if state.failed {
                return std::ptr::null_mut();
            }
            let td = &mut *state.memory.tile_data;
            // The coefficients up to the last one, rounded up to four.
            let coef_count = (i32::from(txb.eob) + 4) >> 2;
            if td.dq_buffer_ptr + (coef_count << 2) > td.dq_buffer_end {
                state.failed = true;
                self.host.no_room(tile, false);
                return std::ptr::null_mut();
            }
            let tx_type = if txb.txtp == WHT_WHT { 0 } else { usize::from(txb.txtp) };
            let tx_size = txb.tx as usize;
            let type_index = if state.lossless && tx_size == 0 { TX_SIZES_ALL } else { tx_size };
            let record = tx_block_info_gpu {
                flags: coef_count as u32
                    | TX_TYPE_FLAGS[tx_type]
                    | (u32::from(txb.plane) << 21)
                    | (TX_TYPE_SCAN[tx_type] << 11),
                input_offset: td.dq_buffer_ptr as u32,
                output_pos: txb.x4 as u32 | ((txb.y4 as u32) << 16),
                sorting_idx: ((td.idct_blocks_sizes[type_index] << 8) + type_index as c_int) as u32,
            };
            let block = td.idct_blocks_host.add(td.idct_blocks_ptr as usize);
            td.idct_blocks_sizes[type_index] += 1;
            td.idct_blocks_ptr += 1;
            let coefs = td.dq_buffer_base.add(td.dq_buffer_ptr as usize);
            let count = (coef_count << 2) as usize;
            td.dq_buffer_ptr += coef_count << 2;
            let Some(stage) = &mut state.stage else {
                block.write(record);
                // The parser writes the coefficients that are not zero, as the entropy decoder of libaom
                // does: the buffer is zero where a frame has not written it (the transform stage binds it
                // for writing and clears what it has read).
                return coefs;
            };
            // Staged: the record and the coefficients go on behind the runs, which go to the buffers
            // first when they have no room left or the buffers do not go on where the runs end (the
            // tile's second part).
            let fits = stage.coef_count + count <= STAGE_COEFS && stage.block_count < STAGE_BLOCKS;
            let goes_on = (stage.coef_count == 0 || stage.coef_dst.add(stage.coef_count) == coefs)
                && (stage.block_count == 0 || stage.block_dst.add(stage.block_count) == block);
            if !(fits && goes_on) {
                Self::flush_stage(&self.host, state);
            }
            let stage = state.stage.as_mut().expect("the tile stages");
            if stage.coef_count == 0 {
                stage.coef_dst = coefs;
            }
            if stage.block_count == 0 {
                stage.block_dst = block;
            }
            stage.blocks[stage.block_count] = record;
            stage.block_count += 1;
            let staged = stage.coefs.as_mut_ptr().add(stage.coef_count);
            stage.coef_count += count;
            staged
        }
    }

    fn superblock_call(&self, _frame: usize, tile: usize, superblock: &ParseSuperblock<'_>) {
        // SAFETY: a call of the tile's thread between the frame's begin and end; the grid is read inside
        // the tile, whose blocks have their records.
        unsafe {
            let frame = self.frame();
            if frame.cdef_coded {
                // libaom keeps the CDEF index of a 64x64 area in the record its top left 4x4 unit points
                // to (`read_cdef` of `av1/decoder/decodemv.c`); -1 when all its blocks are skipped.
                let quadrants = if frame.sb128 { 0..4 } else { 0..1 };
                for quadrant in quadrants {
                    let (x4, y4, index) = if frame.sb128 {
                        (superblock.x4 + (quadrant & 1) * 16, superblock.y4 + (quadrant >> 1) * 16, quadrant as usize)
                    } else {
                        let index = ((superblock.x4 >> 4) & 1) + ((superblock.y4 >> 4) & 1) * 2;
                        (superblock.x4, superblock.y4, index as usize)
                    };
                    if x4 >= frame.mi_cols || y4 >= frame.mi_rows {
                        continue;
                    }
                    let mi = *frame.mi_grid.add((y4 * frame.mi_stride + x4) as usize);
                    if !mi.is_null() {
                        (*mi).cdef_strength = superblock.cdef_idx[index];
                    }
                }
            }
        }
        self.host.superblock(tile, superblock);
        // The next superblock of the tile, if there is one (`decode_tile2`).
        // SAFETY: as above.
        unsafe {
            let frame = self.frame();
            let state = self.tile(tile).as_mut().expect("the tile has begun");
            let side = if frame.sb128 { 32 } else { 16 };
            let last =
                superblock.x4 + side >= state.xd.tile.mi_col_end && superblock.y4 + side >= state.xd.tile.mi_row_end;
            if !last {
                Self::coefs_go_on(&self.host, tile, state);
            }
        }
    }
}

impl<H: SinkHost> ParseSink for Sink<H> {
    fn frame_begin(&self, frame: &ParseFrame<'_>) -> Result<usize, ()> {
        let memory = self.host.frame_begin(frame)?;
        let has_chroma = frame.seq_hdr.layout != Rav1dPixelLayout::I400;
        let ss_x = c_int::from(frame.seq_hdr.layout != Rav1dPixelLayout::I444);
        let ss_y = c_int::from(matches!(frame.seq_hdr.layout, Rav1dPixelLayout::I420 | Rav1dPixelLayout::I400));
        let tiles = usize::from(frame.frame_hdr.tiling.cols) * usize::from(frame.frame_hdr.tiling.rows);
        // SAFETY: no tile of this sink is in parse (the contract of `ParseSink::frame_begin`), so the
        // three fields are this thread's.
        unsafe {
            let pbi = &mut **self.pbi.get();
            pbi.gpu_decoder = self.host.gpu_decoder().cast();
            let cm = &mut pbi.common;
            cm.mi_cols = frame.bw;
            cm.mi_rows = frame.bh;
            cm.mi_stride = memory.mi_stride;
            cm.mi_grid_base = memory.mi_grid;
            cm.mi_grid_visible = memory.mi_grid;
            // BLOCK_128X128 or BLOCK_64X64, and their sides in 4x4 units.
            cm.seq_params.sb_size = if frame.seq_hdr.sb128 != 0 { 15 } else { 12 };
            cm.seq_params.mib_size_log2 = if frame.seq_hdr.sb128 != 0 { 5 } else { 4 };
            cm.seq_params.mib_size = 1 << cm.seq_params.mib_size_log2;
            *self.frame.get() = Some(FrameState {
                hdr: frame.frame_hdr.clone(),
                mi_cols: frame.bw,
                mi_rows: frame.bh,
                mi_stride: memory.mi_stride,
                mi_grid: memory.mi_grid,
                has_chroma,
                ss_x,
                ss_y,
                sb128: frame.seq_hdr.sb128 != 0,
                cdef_coded: !(frame.frame_hdr.all_lossless || frame.frame_hdr.allow_intrabc),
            });
            let states = &mut *self.tiles.get();
            for state in states.iter_mut() {
                // A tile of the frame before keeps its block context for the next frame's tile.
                if let Some(state) = state.get_mut() {
                    state.mi_count = 0;
                    state.mi_count2 = 0;
                    state.second = None;
                    state.failed = false;
                }
            }
            while states.len() < tiles {
                states.push(UnsafeCell::new(None));
            }
        }
        Ok(0)
    }

    fn tile_begin(&self, _frame: usize, tile: &ParseTile) {
        let memory = self.host.tile_begin(tile);
        // SAFETY: a call of the tile's thread between the frame's begin and end.
        unsafe {
            let frame = self.frame();
            let slot = self.tile(tile.index);
            let state = slot.get_or_insert_with(|| TileState {
                xd: zeroed_box(),
                memory: TileMemory {
                    tile_data: std::ptr::null_mut(),
                    mi_pool: std::ptr::null_mut(),
                    mi_count_max: 0,
                    worst_case_coefs: true,
                },
                mi_count: 0,
                second: None,
                mi_count2: 0,
                last_coefs: true,
                lossless: false,
                failed: false,
                stage: None,
                call_stamps: 0,
                begin_ticks: 0,
                begin_stamp: 0,
            });
            if self.options.stage {
                // What a frame that failed has left is not for this frame's buffers.
                state.stage.get_or_insert_with(Stage::new).forget();
            }
            if self.options.time_calls {
                state.call_stamps = 0;
                state.begin_ticks = self.host.ticks();
                state.begin_stamp = time_stamp();
            }
            state.mi_count = 0;
            state.mi_count2 = 0;
            state.second = None;
            state.last_coefs = memory.worst_case_coefs;
            state.failed = false;
            let xd = &mut *state.xd;
            xd.tile = TileInfo {
                mi_row_start: tile.row_start,
                mi_row_end: tile.row_end,
                mi_col_start: tile.col_start,
                mi_col_end: tile.col_end,
                tile_row: 0,
                tile_col: 0,
            };
            xd.tile_data = memory.tile_data;
            xd.mi_stride = frame.mi_stride;
            xd.cur_frame_force_integer_mv = frame.hdr.force_integer_mv.into();
            for (dst, lossless) in xd.lossless.iter_mut().zip(frame.hdr.segmentation.lossless) {
                *dst = lossless.into();
            }
            for plane in &mut xd.plane[1..] {
                plane.subsampling_x = frame.ss_x;
                plane.subsampling_y = frame.ss_y;
            }
            state.memory = memory;
            // The first superblock of the tile (`decode_tile2`).
            Self::coefs_go_on(&self.host, tile.index, state);
        }
    }

    fn block(&self, frame: usize, tile: usize, block: &ParseBlock<'_>) {
        let begin = self.call_begin();
        self.block_call(frame, tile, block);
        // SAFETY: a call of the tile's thread between the frame's begin and end.
        unsafe { self.call_end(tile, begin) };
    }

    fn txb(&self, frame: usize, tile: usize, txb: &ParseTxb) -> *mut i32 {
        let begin = self.call_begin();
        let coefs = self.txb_call(frame, tile, txb);
        // SAFETY: as above.
        unsafe { self.call_end(tile, begin) };
        coefs
    }

    fn palette(&self, _frame: usize, tile: usize, palette: &ParsePalette<'_>) {
        let begin = self.call_begin();
        self.host.palette(tile, palette);
        // SAFETY: as above.
        unsafe { self.call_end(tile, begin) };
    }

    fn superblock(&self, frame: usize, tile: usize, superblock: &ParseSuperblock<'_>) {
        let begin = self.call_begin();
        self.superblock_call(frame, tile, superblock);
        // SAFETY: as above.
        unsafe { self.call_end(tile, begin) };
    }

    fn tile_end(&self, _frame: usize, tile: usize) {
        // SAFETY: a call of the tile's thread between the frame's begin and end. The block context is
        // set for each block as `set_offsets_for_pred_and_recon` and `set_mi_row_col` of the entropy
        // decoder set it; `av1_mi_push_block` reads the frame through `pbi`, which no thread writes
        // while tiles are in parse.
        unsafe {
            let frame = self.frame();
            let state = self.tile(tile).as_mut().expect("the tile has begun");
            // What the tile has staged, and the time of its calls.
            Self::flush_stage(&self.host, state);
            if self.options.time_calls && state.begin_ticks != 0 {
                let stamps = time_stamp().wrapping_sub(state.begin_stamp);
                let ticks = self.host.ticks() - state.begin_ticks;
                if stamps != 0 {
                    let calls = state.call_stamps as f64 * ticks as f64 / stamps as f64;
                    self.host.add_time(SinkTimer::Calls, calls as i64);
                }
            }
            let lists_start = self.host.ticks();
            let td = state.memory.tile_data;
            (*td).mi_count = state.mi_count;
            if let Some(second) = &state.second {
                (*second.tile_data).mi_count = state.mi_count2;
            }
            let pbi: *mut AV1Decoder = &raw mut **self.pbi.get();
            let cm = &raw mut (*pbi).common;
            let xd: *mut MACROBLOCKD = &raw mut *state.xd;
            let (row_start, col_start) = ((*xd).tile.mi_row_start, (*xd).tile.mi_col_start);
            let start = Instant::now();
            // The records of the first part, then those of the second.
            let second_pool = state.second.as_ref().map_or(std::ptr::null_mut(), |second| second.mi_pool);
            let records = (0..state.mi_count as usize)
                .map(|index| state.memory.mi_pool.add(index))
                .chain((0..state.mi_count2 as usize).map(|index| second_pool.add(index)));
            for mi in records {
                let mi = &*mi;
                let (mi_row, mi_col) = (mi.mi_row, mi.mi_col);
                let bw = 1 << MI_SIZE_WIDE_LOG2[mi.sb_type as usize];
                let bh = 1 << MI_SIZE_HIGH_LOG2[mi.sb_type as usize];
                (*xd).mi = frame.mi_grid.add((mi_row * frame.mi_stride + mi_col) as usize);
                (*xd).mb_to_bottom_edge = (frame.mi_rows - bh - mi_row) * 4 * 8;
                (*xd).mb_to_right_edge = (frame.mi_cols - bw - mi_col) * 4 * 8;
                let up = mi_row > row_start;
                let left = mi_col > col_start;
                (*xd).up_available = up.into();
                (*xd).left_available = left.into();
                (*xd).chroma_up_available =
                    if frame.ss_y != 0 && bh < 2 { (mi_row - 1 > row_start).into() } else { up.into() };
                (*xd).chroma_left_available =
                    if frame.ss_x != 0 && bw < 2 { (mi_col - 1 > col_start).into() } else { left.into() };
                av1_mi_push_block(pbi, cm, xd);
            }
            self.block_list_nanos.fetch_add(start.elapsed().as_nanos() as u64, Ordering::Relaxed);
            if lists_start != 0 {
                self.host.add_time(SinkTimer::Lists, self.host.ticks() - lists_start);
            }
        }
        self.host.tile_end(tile);
    }

    fn frame_end(&self, _frame: usize, ok: bool) -> bool {
        // SAFETY: no tile of the frame is in parse any more.
        let failed = unsafe {
            let failed =
                (&mut *self.tiles.get()).iter_mut().any(|tile| tile.get_mut().as_ref().is_some_and(|t| t.failed));
            *self.frame.get() = None;
            failed
        };
        // Both are called, whatever the tiles say: the host has to take the frame back.
        let stands = self.host.frame_end(ok && !failed);
        stands && !failed
    }

    fn sequence(&self, seq_hdr: &Rav1dSequenceHeader, new: bool) -> Result<(), ()> {
        self.host.sequence(seq_hdr, new)
    }

    fn show_existing(&self, show: &ParseShowExisting<'_>) -> Result<(), ()> {
        self.host.show_existing(show)
    }

    fn ticks(&self) -> i64 {
        self.host.ticks()
    }

    fn tile_time(&self, _frame: usize, start: i64) {
        self.host.tile_time(start);
    }
}
