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
//! - the block lists of the prediction stages, from the records in the order they were read:
//!   `av1_mi_push_block`, the host's function, called with the block context the entropy decoder calls
//!   it with (`decode_tile2`). The entropy decoder counts the lists of a tile when the tile is parsed;
//!   the sink counts those of a superblock when the superblock is parsed, while its records and its
//!   part of the grid are in the cache ([`SinkOptions::late_lists`] for the other way). The function
//!   reads the block's record and, through the grid, records of blocks above it and to its left, which
//!   are read before it; the calls come in the same order, so the lists are the same.
//!
//! The memory is the host's ([`SinkHost`]): for the D3D12 host the mapped buffers of a pipeline slot and
//! of the decoder's pools, for `tools/rav1g-parse-check` plain memory. A tile gets its mode info records,
//! its coefficients and its transform block records in pieces: when the piece it writes is full, the
//! sink asks the host for the next one (`more_mode_info`, `more_coefs`, `more_transform_blocks`), which
//! may be anywhere in the buffer (docs/phase2-rav1d.md, step 5). A host that gives a tile room for all
//! it can have at its begin is never asked.
//!
//! Two of those buffers are write-combined memory with the D3D12 host (the coefficients and the
//! transform block records: upload heaps, which the CPU must not read and should write in order). The
//! sink writes them as the entropy decoder of libaom does: each record once, in order, and of the
//! coefficients the ones that are not zero, in rising scan order.
//!
//! The block lists are computed by code that takes its block context from the structs of libaom's
//! headers, as it did while that entropy decoder was the parser (docs/phase2-rav1d.md, step 6: a block
//! context of the host's own is left to do). So the sink keeps such
//! structs of its own, cleared, and sets the fields that code reads: an `AV1_COMMON` for the frame (frame
//! size in 4x4 units, grid, superblock size) and a `MACROBLOCKD` per tile (the block's place in the grid,
//! what is available around it, the tile, whose data names the frame's slot at the host).
//!
//! **Frames are values** (docs/phase2-rav1d.md, step 4). The sink keeps the state of every frame in parse
//! by itself, in as many places as frames may be in parse at once ([`Sink::with_frames`]): `frame_begin`
//! takes a free one, its number goes to the host and comes back with every call of the frame, and
//! `frame_end` gives it back. Nothing of the sink is "the frame in parse": the tiles of two frames are
//! parsed side by side.

use std::alloc::{Layout, alloc_zeroed, handle_alloc_error};
use std::cell::UnsafeCell;
use std::ffi::c_int;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use rav1d::include::dav1d::headers::Rav1dSequenceHeader;
use rav1d::include::dav1d::headers::{Rav1dFrameHeader, Rav1dPixelLayout};
use rav1d::parse::{
    ParseBlock, ParseColorCodes, ParseFrame, ParsePalette, ParseShowExisting, ParseSink, ParseSuperblock, ParseTile,
    ParseTxb,
};
use rav1g_sys::parser::{
    AV1_COMMON, AV1Decoder, MACROBLOCKD, MB_MODE_INFO, TileInfo, av1_tile_data, tx_block_info_gpu,
};

/// `SinkOptions::test_panic`: the frame (counted from 1) whose first superblock panics.
const TEST_PANIC_FRAME: u64 = 3;

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

/// How the sink works. All off is the sink as it writes by default.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SinkOptions {
    /// The block lists of a tile are counted when the tile is parsed, in one pass over its records, as
    /// the entropy decoder of libaom counts them, and not superblock by superblock. The lists are the
    /// same; a tile's records are then mostly out of the cache when they are read again (4.7 ms of a
    /// heavy 4K frame on the console, docs/phase2-rav1d.md).
    pub late_lists: bool,
    /// The time inside the calls of a tile (block, transform block, palette, superblock) is measured
    /// and given to the host ([`SinkTimer::Calls`]), when the host has a clock. Two readings of the
    /// processor's time stamp counter a call.
    pub time_calls: bool,
    /// For the test of what a bug does: the sink panics at the end of the first superblock of the third
    /// frame it is given, on the thread that parses it. Nothing else sets this.
    pub test_panic: bool,
}

/// What the sink measures for a host with a clock ([`SinkHost::add_time`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SinkTimer {
    /// The calls of a tile: the mode info record and the grid of a block, the record of a transform
    /// block, the palette, the superblock. Only with [`SinkOptions::time_calls`]. The parse that makes the calls is not in it, and neither
    /// are the coefficients the parse writes itself.
    Calls,
    /// The block lists: `av1_mi_push_block` for every record, superblock by superblock or, with
    /// [`SinkOptions::late_lists`], at the end of a tile.
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
    /// The tile's cursors into the buffers of the frame, set up as `av1_setup_macroblockd` sets them up:
    /// where its coefficients go and where they end (`dq_buffer_ptr`, `dq_buffer_end`), and where its
    /// transform block records go (`idct_blocks_host`, `idct_blocks_ptr`).
    pub tile_data: *mut av1_tile_data,
    /// The tile's first piece of mode info records and the records it has room for; 0 records: the
    /// host gives the first piece when the first block asks for it.
    pub mi_pool: *mut MB_MODE_INFO,
    pub mi_count_max: c_int,
    /// The transform block records there is room for where the tile's cursor is.
    pub txb_room: c_int,
}

/// A piece of mode info records for a tile ([`SinkHost::more_mode_info`]): the records and how many
/// there is room for.
pub struct ModeInfoPiece {
    pub mi_pool: *mut MB_MODE_INFO,
    pub count: c_int,
}

/// What gives the sink its memory: the host of the decoder.
///
/// `frame` is the number the sink gave the frame: one of `0..frames` of [`Sink::with_frames`], taken at
/// the frame's begin and free again when its `frame_end` has returned. The calls of a frame come between
/// its `frame_begin` and its `frame_end`; with more than one frame in parse, `frame_begin` comes on the
/// thread that sends the data, in decode order, and everything else of a frame on the threads that parse
/// it, next to the calls of other frames (`rav1d::parse::ParseSink`).
pub trait SinkHost: Send + Sync {
    /// A sequence header (`ParseSink::sequence`). `Err` refuses the sequence.
    #[expect(clippy::result_unit_err, reason = "the error of ParseSink::sequence, which has no more to say")]
    fn sequence(&self, _seq_hdr: &Rav1dSequenceHeader, _color: ParseColorCodes, _new: bool) -> Result<(), ()> {
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

    /// A superblock row of a tile of the frame was parsed since `start`, with the sink's part of it.
    fn tile_time(&self, _frame: usize, _start: i64) {}

    /// A part of the sink's work for the frame took `ticks` of the clock of [`Self::ticks`]. On a tile's
    /// thread.
    fn add_time(&self, _frame: usize, _timer: SinkTimer, _ticks: i64) {}

    /// A frame's parse starts. `Err` fails the frame.
    #[expect(clippy::result_unit_err, reason = "the error of ParseSink::frame_begin, which has no more to say")]
    fn frame_begin(&self, frame: usize, header: &ParseFrame<'_>) -> Result<FrameMemory, ()>;

    /// A tile's parse starts, on the thread that parses it.
    fn tile_begin(&self, frame: usize, tile: &ParseTile) -> TileMemory;

    /// The tile's piece of mode info records is full (or it has none yet): the piece its records go
    /// on in. The call may wait for memory that an older frame gives back. `None` when there is none:
    /// the frame fails. `tile_data.mi_count` has the records of the piece that is full.
    fn more_mode_info(&self, frame: usize, tile: usize, tile_data: *mut av1_tile_data) -> Option<ModeInfoPiece>;

    /// The coefficients of the tile's next transform block do not fit what is left of its piece
    /// (`dq_buffer_end`): the host moves the tile's cursor (`dq_buffer_ptr`, `dq_buffer_end`) to where
    /// they go on, a piece with room for the coefficients of any transform block. May wait like
    /// [`Self::more_mode_info`]. `false` when there is none: the frame fails.
    fn more_coefs(&self, frame: usize, tile: usize, tile_data: *mut av1_tile_data) -> bool;

    /// The tile's piece of transform block records is full (or it has none yet): the host moves the
    /// tile's cursor (`idct_blocks_host`, `idct_blocks_ptr`) to the next piece and returns the records
    /// it has room for. May wait like [`Self::more_mode_info`]. 0 when there is none: the frame fails.
    fn more_transform_blocks(&self, frame: usize, tile: usize, tile_data: *mut av1_tile_data) -> c_int;

    /// The colour index map of a palette block's plane (`av1_intra_palette`).
    fn palette(&self, frame: usize, tile: usize, palette: &ParsePalette<'_>);

    /// A superblock is parsed: its CDEF indexes and loop restoration units.
    fn superblock(&self, frame: usize, tile: usize, superblock: &ParseSuperblock<'_>);

    /// A tile is parsed and its block lists are counted.
    fn tile_end(&self, frame: usize, tile: usize);

    /// The frame's parse has ended. Returns whether the frame stands (`ParseSink::frame_end`). With more
    /// than one frame in parse this is a call of a parser thread that holds the parser's lock: the host
    /// hands the frame on and returns.
    fn frame_end(&self, frame: usize, ok: bool) -> bool;
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
    /// [`SinkOptions::test_panic`]: this is the frame that panics.
    panics: bool,
}

/// What the sink keeps of a tile in parse. One thread at a time works on a tile.
struct TileState {
    /// The block context `av1_mi_push_block` reads, cleared, with the tile's constants set.
    xd: Box<MACROBLOCKD>,
    memory: TileMemory,
    /// The pieces of the tile's mode info records in the order they were written; the last one is the
    /// one in use, the others are full.
    pieces: Vec<Records>,
    /// The transform block records there is room for where the tile's cursor is.
    txb_room: c_int,
    /// The segment of the tile's last block is lossless.
    lossless: bool,
    /// The tile ran out of room: its frame fails.
    failed: bool,
    /// The records of the tile whose block lists are counted: the piece, and the records of that piece.
    listed: (usize, c_int),
    /// For a host with a clock: the time stamps the calls of the tile took ([`SinkOptions::time_calls`])
    /// and those its block lists took, and the clock and the time stamp at the tile's begin, which give
    /// the length of a time stamp.
    call_stamps: u64,
    list_stamps: u64,
    begin_ticks: i64,
    begin_stamp: u64,
}

/// A piece of a tile's mode info records.
struct Records {
    pool: *mut MB_MODE_INFO,
    /// The records that are written, and those there is room for.
    count: c_int,
    max: c_int,
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

/// What the sink keeps for one of the frames that may be in parse at once.
struct FramePlace {
    /// A frame has it: taken by `frame_begin` (the thread that sends the data), given back when the
    /// frame's `frame_end` has returned (any thread).
    taken: AtomicBool,
    /// Written by `frame_begin`, while no tile of the frame is in parse; read by the tiles.
    frame: UnsafeCell<Option<FrameState>>,
    /// The frame for `av1_mi_push_block`; written like `frame`.
    cm: UnsafeCell<Box<AV1_COMMON>>,
    /// The tiles of the frame; the vector is written by `frame_begin`, an element by its tile's thread.
    tiles: UnsafeCell<Vec<UnsafeCell<Option<TileState>>>>,
}

/// The sink.
pub struct Sink<H: SinkHost> {
    host: H,
    /// One place for every frame that may be in parse at once.
    frames: Box<[FramePlace]>,
    /// The frames that have begun.
    begun: AtomicU64,
    options: SinkOptions,
}

// SAFETY: the calls of a `ParseSink` come as its contract says: `frame_begin` and `frame_end` while no
// tile of the frame is in parse, the calls of a tile from one thread at a time. The fields of a frame's
// place are written under that contract (see their comments), and a place is one frame's from its begin to
// its end (`taken`); the raw pointers in them are the host's memory, which the host gives for the frame.
unsafe impl<H: SinkHost> Send for Sink<H> {}
// SAFETY: as above.
unsafe impl<H: SinkHost> Sync for Sink<H> {}

impl<H: SinkHost> Sink<H> {
    pub fn new(host: H) -> Self {
        Self::with_options(host, SinkOptions::default())
    }

    /// A sink for a parser with one frame in parse at a time.
    pub fn with_options(host: H, options: SinkOptions) -> Self {
        Self::with_frames(host, options, 1)
    }

    /// A sink for a parser with up to `frames` frames in parse at once (`ParseSettings::frames`).
    pub fn with_frames(host: H, options: SinkOptions, frames: usize) -> Self {
        let frames = (0..frames.max(1))
            .map(|_| FramePlace {
                taken: AtomicBool::new(false),
                frame: UnsafeCell::new(None),
                cm: UnsafeCell::new(zeroed_box()),
                tiles: UnsafeCell::new(Vec::new()),
            })
            .collect();
        Self { host, frames, begun: AtomicU64::new(0), options }
    }

    /// The frames that may be in parse at once.
    pub fn frames(&self) -> usize {
        self.frames.len()
    }

    pub fn options(&self) -> SinkOptions {
        self.options
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
    unsafe fn call_end(&self, frame: usize, tile: usize, begin: u64) {
        if begin != 0 {
            // SAFETY: the caller's contract.
            if let Some(state) = unsafe { self.tile(frame, tile) } {
                state.call_stamps += time_stamp().wrapping_sub(begin);
            }
        }
    }

    pub fn host(&self) -> &H {
        &self.host
    }

    /// A frame in parse.
    ///
    /// # Safety
    /// Between the `frame_begin` and the `frame_end` of the frame with this number.
    unsafe fn frame(&self, frame: usize) -> &FrameState {
        // SAFETY: the caller's contract: the frame was set by `frame_begin` and is not written until
        // `frame_end`.
        unsafe { (*self.frames[frame].frame.get()).as_ref().expect("the frame is in parse") }
    }

    /// The state of a tile of a frame in parse.
    ///
    /// # Safety
    /// As [`Self::frame`], from the thread that parses the tile.
    #[expect(clippy::mut_from_ref, reason = "a tile's state is its thread's, by the contract of ParseSink")]
    unsafe fn tile(&self, frame: usize, tile: usize) -> &mut Option<TileState> {
        // SAFETY: the caller's contract: the vector is not written while tiles of the frame are in
        // parse, and the element is this thread's.
        unsafe { &mut *(&*self.frames[frame].tiles.get())[tile].get() }
    }
}

/// The calls of a tile, without their timing.
impl<H: SinkHost> Sink<H> {
    /// Counts the block lists of the records of a tile that are not counted yet: all that were read
    /// since the last call, in the order they were read.
    ///
    /// # Safety
    /// The tile's thread, inside the frame. The block context is set for each block as
    /// `set_offsets_for_pred_and_recon` and `set_mi_row_col` of the entropy decoder set it;
    /// `av1_mi_push_block` reads the size of the frame through `cm`, which no thread writes while tiles
    /// of the frame are in parse.
    unsafe fn count_lists(&self, frame_number: usize, tile: usize) {
        // SAFETY: the caller's contract; the records are those the tile has written into its parts of
        // the pool, and each is in the grid.
        unsafe {
            let state = self.tile(frame_number, tile).as_mut().expect("the tile has begun");
            let (mut piece, mut index) = state.listed;
            // Nothing to count: the piece the count stopped in has no new record and is the last one.
            if state.pieces.get(piece).is_none_or(|records| index >= records.count && piece + 1 == state.pieces.len()) {
                return;
            }
            let frame = self.frame(frame_number);
            let stamp = if state.begin_ticks != 0 { time_stamp() } else { 0 };
            // The function finds the frame's slot through the tile's data and does not look at the
            // decoder.
            let pbi: *mut AV1Decoder = std::ptr::null_mut();
            let cm: *mut AV1_COMMON = &raw mut **self.frames[frame_number].cm.get();
            let xd: *mut MACROBLOCKD = &raw mut *state.xd;
            let (row_start, col_start) = ((*xd).tile.mi_row_start, (*xd).tile.mi_col_start);
            // The records piece by piece, in the order they were written. A piece that is not the last
            // one is full.
            loop {
                let records = &state.pieces[piece];
                if index >= records.count {
                    if piece + 1 == state.pieces.len() {
                        break;
                    }
                    piece += 1;
                    index = 0;
                    continue;
                }
                let mi = &*records.pool.add(index as usize);
                index += 1;
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
            state.listed = (piece, index);
            if stamp != 0 {
                state.list_stamps += time_stamp().wrapping_sub(stamp);
            }
        }
    }

    fn block_call(&self, frame_number: usize, tile: usize, block: &ParseBlock<'_>) {
        // SAFETY: a call of the tile's thread between the frame's begin and end; the record is the next
        // one of the tile's part of the pool, and the grid is written inside the tile.
        unsafe {
            let frame = self.frame(frame_number);
            let state = self.tile(frame_number, tile).as_mut().expect("the tile has begun");
            if state.failed {
                return;
            }
            // The tile's piece of the pool, and when it is full the next one (`decode_mbmi_block`,
            // `set_offsets`).
            if state.pieces.last().is_none_or(|records| records.count >= records.max) {
                let td = state.memory.tile_data;
                (*td).mi_count = state.pieces.last().map_or(0, |records| records.count);
                match self.host.more_mode_info(frame_number, tile, td) {
                    Some(piece) if piece.count > 0 => {
                        state.pieces.push(Records { pool: piece.mi_pool, count: 0, max: piece.count });
                    }
                    _ => {
                        state.failed = true;
                        return;
                    }
                }
            }
            let records = state.pieces.last_mut().expect("the tile has a piece");
            let mi = records.pool.add(records.count as usize);
            records.count += 1;
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

    fn txb_call(&self, frame_number: usize, tile: usize, txb: &ParseTxb) -> *mut i32 {
        // SAFETY: a call of the tile's thread between the frame's begin and end; the record and the
        // coefficients are inside the tile's parts of their buffers (checked against their ends).
        unsafe {
            let state = self.tile(frame_number, tile).as_mut().expect("the tile has begun");
            if state.failed {
                return std::ptr::null_mut();
            }
            let td = state.memory.tile_data;
            // The coefficients up to the last one, rounded up to four. They are not split: when they do
            // not fit what is left of the tile's piece, they go into the next one.
            let coef_count = (i32::from(txb.eob) + 4) >> 2;
            if (*td).dq_buffer_ptr + (coef_count << 2) > (*td).dq_buffer_end
                && (!self.host.more_coefs(frame_number, tile, td)
                    || (*td).dq_buffer_ptr + (coef_count << 2) > (*td).dq_buffer_end)
            {
                state.failed = true;
                return std::ptr::null_mut();
            }
            // The record's place: the tile's piece of records, or the next one.
            if state.txb_room <= 0 {
                state.txb_room = self.host.more_transform_blocks(frame_number, tile, td);
                if state.txb_room <= 0 {
                    state.failed = true;
                    return std::ptr::null_mut();
                }
            }
            state.txb_room -= 1;
            let td = &mut *td;
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
            td.dq_buffer_ptr += coef_count << 2;
            block.write(record);
            // The parser writes the coefficients that are not zero, as the entropy decoder of libaom
            // does: the buffer is zero where a frame has not written it (the transform stage binds it
            // for writing and clears what it has read).
            coefs
        }
    }

    fn superblock_call(&self, frame_number: usize, tile: usize, superblock: &ParseSuperblock<'_>) {
        // SAFETY: a call of the tile's thread between the frame's begin and end; the grid is read inside
        // the tile, whose blocks have their records.
        unsafe {
            let frame = self.frame(frame_number);
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
        self.host.superblock(frame_number, tile, superblock);
    }
}

impl<H: SinkHost> ParseSink for Sink<H> {
    fn frame_begin(&self, header: &ParseFrame<'_>) -> Result<usize, ()> {
        // A place for the frame: the parser begins a frame only when fewer frames than it may have in
        // parse are between their begin and their end, so one is free.
        let number = self
            .frames
            .iter()
            .position(|place| place.taken.compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire).is_ok())
            .ok_or(())?;
        let place = &self.frames[number];
        let memory = match self.host.frame_begin(number, header) {
            Ok(memory) => memory,
            Err(()) => {
                place.taken.store(false, Ordering::Release);
                return Err(());
            }
        };
        let has_chroma = header.seq_hdr.layout != Rav1dPixelLayout::I400;
        let ss_x = c_int::from(header.seq_hdr.layout != Rav1dPixelLayout::I444);
        let ss_y = c_int::from(matches!(header.seq_hdr.layout, Rav1dPixelLayout::I420 | Rav1dPixelLayout::I400));
        let tiles = usize::from(header.frame_hdr.tiling.cols) * usize::from(header.frame_hdr.tiling.rows);
        // SAFETY: the place is this frame's and none of its tiles is in parse (the contract of
        // `ParseSink::frame_begin`), so the three fields are this thread's.
        unsafe {
            let cm = &mut **place.cm.get();
            cm.mi_cols = header.bw;
            cm.mi_rows = header.bh;
            cm.mi_stride = memory.mi_stride;
            cm.mi_grid_base = memory.mi_grid;
            cm.mi_grid_visible = memory.mi_grid;
            // BLOCK_128X128 or BLOCK_64X64, and their sides in 4x4 units.
            cm.seq_params.sb_size = if header.seq_hdr.sb128 != 0 { 15 } else { 12 };
            cm.seq_params.mib_size_log2 = if header.seq_hdr.sb128 != 0 { 5 } else { 4 };
            cm.seq_params.mib_size = 1 << cm.seq_params.mib_size_log2;
            *place.frame.get() = Some(FrameState {
                hdr: header.frame_hdr.clone(),
                mi_cols: header.bw,
                mi_rows: header.bh,
                mi_stride: memory.mi_stride,
                mi_grid: memory.mi_grid,
                has_chroma,
                ss_x,
                ss_y,
                sb128: header.seq_hdr.sb128 != 0,
                cdef_coded: !(header.frame_hdr.all_lossless || header.frame_hdr.allow_intrabc),
                panics: self.options.test_panic && self.begun.fetch_add(1, Ordering::Relaxed) + 1 == TEST_PANIC_FRAME,
            });
            let states = &mut *place.tiles.get();
            for state in states.iter_mut() {
                // A tile of the frame before keeps its block context for the next frame's tile.
                if let Some(state) = state.get_mut() {
                    state.pieces.clear();
                    state.failed = false;
                }
            }
            while states.len() < tiles {
                states.push(UnsafeCell::new(None));
            }
        }
        Ok(number)
    }

    fn tile_begin(&self, frame_number: usize, tile: &ParseTile) {
        let memory = self.host.tile_begin(frame_number, tile);
        // SAFETY: a call of the tile's thread between the frame's begin and end.
        unsafe {
            let frame = self.frame(frame_number);
            let slot = self.tile(frame_number, tile.index);
            let state = slot.get_or_insert_with(|| TileState {
                xd: zeroed_box(),
                memory: TileMemory {
                    tile_data: std::ptr::null_mut(),
                    mi_pool: std::ptr::null_mut(),
                    mi_count_max: 0,
                    txb_room: 0,
                },
                pieces: Vec::new(),
                txb_room: 0,
                lossless: false,
                failed: false,
                listed: (0, 0),
                call_stamps: 0,
                list_stamps: 0,
                begin_ticks: 0,
                begin_stamp: 0,
            });
            // A host with a clock (a timing run): what a time stamp is worth is taken over the tile.
            state.call_stamps = 0;
            state.list_stamps = 0;
            state.begin_ticks = self.host.ticks();
            state.begin_stamp = if state.begin_ticks != 0 { time_stamp() } else { 0 };
            state.listed = (0, 0);
            state.pieces.clear();
            if memory.mi_count_max > 0 {
                state.pieces.push(Records { pool: memory.mi_pool, count: 0, max: memory.mi_count_max });
            }
            state.txb_room = memory.txb_room;
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
        }
    }

    fn block(&self, frame: usize, tile: usize, block: &ParseBlock<'_>) {
        let begin = self.call_begin();
        self.block_call(frame, tile, block);
        // SAFETY: a call of the tile's thread between the frame's begin and end.
        unsafe { self.call_end(frame, tile, begin) };
    }

    fn txb(&self, frame: usize, tile: usize, txb: &ParseTxb) -> *mut i32 {
        let begin = self.call_begin();
        let coefs = self.txb_call(frame, tile, txb);
        // SAFETY: as above.
        unsafe { self.call_end(frame, tile, begin) };
        coefs
    }

    fn palette(&self, frame: usize, tile: usize, palette: &ParsePalette<'_>) {
        let begin = self.call_begin();
        self.host.palette(frame, tile, palette);
        // SAFETY: as above.
        unsafe { self.call_end(frame, tile, begin) };
    }

    fn superblock(&self, frame: usize, tile: usize, superblock: &ParseSuperblock<'_>) {
        let begin = self.call_begin();
        self.superblock_call(frame, tile, superblock);
        // SAFETY: as above.
        unsafe { self.call_end(frame, tile, begin) };
        // The block lists of the superblock, while its records are at hand. They have a timer of their
        // own, so they are not inside the timed call.
        if !self.options.late_lists {
            // SAFETY: as above.
            unsafe { self.count_lists(frame, tile) };
        }
        // SAFETY: as above.
        if unsafe { self.frame(frame) }.panics {
            panic!("the test of a panic in the parse of a frame (SinkOptions::test_panic)");
        }
    }

    fn tile_end(&self, frame: usize, tile: usize) {
        // SAFETY: a call of the tile's thread between the frame's begin and end.
        unsafe {
            {
                let state = self.tile(frame, tile).as_mut().expect("the tile has begun");
                // The records of the tile's last piece; the pieces before it are full.
                let td = state.memory.tile_data;
                (*td).mi_count = state.pieces.last().map_or(0, |records| records.count);
            }
            // The block lists that are not counted yet: all of the tile's with
            // `SinkOptions::late_lists`, and otherwise none (the tile's last superblock counted them).
            self.count_lists(frame, tile);
            // The timers of the tile, for a host with a clock.
            let state = self.tile(frame, tile).as_mut().expect("the tile has begun");
            if state.begin_ticks != 0 {
                let stamps = time_stamp().wrapping_sub(state.begin_stamp);
                let ticks = self.host.ticks() - state.begin_ticks;
                if stamps != 0 {
                    let ticks_a_stamp = ticks as f64 / stamps as f64;
                    if self.options.time_calls {
                        self.host.add_time(frame, SinkTimer::Calls, (state.call_stamps as f64 * ticks_a_stamp) as i64);
                    }
                    self.host.add_time(frame, SinkTimer::Lists, (state.list_stamps as f64 * ticks_a_stamp) as i64);
                }
            }
        }
        self.host.tile_end(frame, tile);
    }

    fn frame_end(&self, frame: usize, ok: bool) -> bool {
        let place = &self.frames[frame];
        // SAFETY: no tile of the frame is in parse any more, and the place is the frame's until it is
        // given back below.
        let failed = unsafe {
            let failed =
                (&mut *place.tiles.get()).iter_mut().any(|tile| tile.get_mut().as_ref().is_some_and(|t| t.failed));
            *place.frame.get() = None;
            failed
        };
        // Both are called, whatever the tiles say: the host has to take the frame back.
        let stands = self.host.frame_end(frame, ok && !failed);
        place.taken.store(false, Ordering::Release);
        stands && !failed
    }

    fn sequence(&self, seq_hdr: &Rav1dSequenceHeader, color: ParseColorCodes, new: bool) -> Result<(), ()> {
        self.host.sequence(seq_hdr, color, new)
    }

    fn show_existing(&self, show: &ParseShowExisting<'_>) -> Result<(), ()> {
        self.host.show_existing(show)
    }

    fn ticks(&self) -> i64 {
        self.host.ticks()
    }

    fn tile_time(&self, frame: usize, start: i64) {
        self.host.tile_time(frame, start);
    }
}
