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
//!   part of the grid are in the cache (or, with [`SinkOptions::pipe_lists`], a superblock row behind it).
//!   The function
//!   reads the block's record and, through the grid, records of blocks above it and to its left, which
//!   are read before it; the calls come in the same order, so the lists are the same.
//!
//! With [`SinkOptions::gpu_grid`] the sink writes no grid: the host builds it from the records (a pass of
//! the GPU). What the parse threads read through the grid, the sink then keeps by itself, a few KB a tile
//! that stay in the cache: for the block lists the records at the edges of the tile, as the entropy
//! decoder keeps its contexts above and to the left (the lowest block of every column and the rightmost
//! block of every row of the superblock row; `TileState::above`, `TileState::left`), and for the CDEF
//! index of a 64x64 area the record of the block at its top left.
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
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Mutex, PoisonError, RwLock};

use rav1d::include::dav1d::headers::Rav1dSequenceHeader;
use rav1d::include::dav1d::headers::{Rav1dFrameHeader, Rav1dPixelLayout};
use rav1d::parse::{
    ParseBlock, ParseColorCodes, ParseFrame, ParsePalette, ParseShowExisting, ParseSink, ParseSuperblock, ParseTile,
    ParseTxb,
};
use rav1g_sys::{
    AV1_COMMON, AV1Decoder, MACROBLOCKD, MB_MODE_INFO, MI_LEFT_EDGE, MiGridEntry, TileInfo, av1_tile_data,
    mi_grid_entry, mi_grid_record_in_chunks, tx_block_info_gpu,
};

/// `SinkOptions::test_panic`: the frame (counted from 1) whose first superblock panics.
const TEST_PANIC_FRAME: u64 = 3;

use crate::record::{self, BLOCK_SIZE_OF, MI_SIZE_HIGH_LOG2, MI_SIZE_WIDE_LOG2, RecordFrame};

unsafe extern "C" {
    /// `dx/av1_core.h`: counts the prediction blocks of the block `xd` is at into the lists of its tile.
    /// The host's function (`crates/rav1g/src/host/blocks.rs`).
    fn av1_mi_push_block(pbi: *mut AV1Decoder, cm: *mut AV1_COMMON, xd: *mut MACROBLOCKD);
    /// The same file: the time stamps the wavefront and intra part of `av1_mi_push_block` took on this
    /// thread since the last call, which starts the count again; `count` says whether the calls that
    /// follow are counted ([`SinkOptions::time_calls`]).
    fn rav1g_intra_list_stamps(count: bool) -> u64;
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
    /// The time inside the calls of a tile (block, transform block, palette, superblock) is measured
    /// and given to the host ([`SinkTimer::Calls`]), when the host has a clock. Two readings of the
    /// processor's time stamp counter a call.
    pub time_calls: bool,
    /// For the test of what a bug does: the sink panics at the end of the first superblock of the third
    /// frame it is given, on the thread that parses it. Nothing else sets this.
    pub test_panic: bool,
    /// The sink does not write the grid ([`FrameMemory::mi_grid`] is not looked at): the host builds it
    /// from the records. The records the block lists read above and to the left of a block, and the
    /// record a 64x64 area keeps its CDEF index in, come from what the sink keeps of a tile. The records
    /// and the lists are the same.
    pub gpu_grid: bool,
    /// The block lists of a tile are counted a superblock row behind its entropy decoder, by a parse
    /// thread that has no task of the parser (`ParseSink::idle_work`), or by the tile's thread itself
    /// when a row has waited for one a row long, and the rest of them when the tile is through
    /// (`ParseSink::tile_finish`), behind the save of the frame's CDFs. For frames whose tiles are
    /// parsed by the worker threads (`ParseFrame::tile_tasks`); otherwise, and off, the tile's thread
    /// counts them superblock by superblock. The lists are the same.
    pub pipe_lists: bool,
}

/// What the sink measures for a host with a clock ([`SinkHost::add_time`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SinkTimer {
    /// The calls of a tile: the mode info record and the grid of a block, the record of a transform
    /// block, the palette, the superblock. Only with [`SinkOptions::time_calls`]. The parse that makes the calls is not in it, and neither
    /// are the coefficients the parse writes itself.
    Calls,
    /// The block lists: `av1_mi_push_block` for every record, superblock by superblock or, with
    /// [`SinkOptions::pipe_lists`], a superblock row behind.
    Lists,
    /// Of [`SinkTimer::Calls`]: the entries of the grid a block call writes. Only with
    /// [`SinkOptions::time_calls`].
    Grid,
    /// Of [`SinkTimer::Lists`]: the wavefront and the lists of intra prediction, the part of
    /// `av1_mi_push_block` behind the inter blocks' lists. Only with [`SinkOptions::time_calls`].
    ListsIntra,
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
    /// The grid: the entry of the block's record at every 4x4 unit (`rav1g_sys::mi_grid_entry`),
    /// `mi_stride` a row. Not written, and not read, with [`SinkOptions::gpu_grid`].
    pub mi_grid: *mut MiGridEntry,
    pub mi_stride: c_int,
    /// The mapping of each logical mode info chunk when the CPU writes the grid.
    pub mi_chunks: *const std::cell::Cell<*mut MB_MODE_INFO>,
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
    mi_grid: *mut MiGridEntry,
    mi_chunks: *const std::cell::Cell<*mut MB_MODE_INFO>,
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
    /// [`SinkOptions::pipe_lists`] for this frame.
    pipe_lists: bool,
}

/// What the sink keeps of a tile in parse. One thread at a time works on a tile.
struct TileState {
    /// The tile's index in its frame, and the frame's state; set at the tile's begin. The calls of the
    /// tile's blocks come with the address of this state ([`ParseSink::tile_begin`]), so that they
    /// find the tile and its frame without looking them up.
    index: usize,
    frame: *const FrameState,
    /// The tile's block lists, in its frame's place; set at the tile's begin.
    lists: *const TileLists,
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
    /// The tile's last superblock row was parsed (`tile_end`), and the tile is through
    /// (`tile_finish`).
    ended: bool,
    finished: bool,
    /// Where the tile's superblock rows end, in 4x4 units, and the width of a superblock.
    col_end: c_int,
    sb_size: c_int,
    /// For a host with a clock: the time stamps the calls of the tile took ([`SinkOptions::time_calls`]),
    /// and the clock and the time stamp at the tile's begin, which give the length of a time stamp.
    call_stamps: u64,
    /// Part of it with [`SinkOptions::time_calls`]: the grid's entries.
    grid_stamps: u64,
    begin_ticks: i64,
    begin_stamp: u64,
    /// With [`SinkOptions::gpu_grid`]: the records of the blocks at the top left of the 64x64 areas of
    /// the superblock in parse (four areas of a 128x128 superblock, by column and row; the first entry
    /// alone for a 64x64 one), null where no block has been read: where libaom keeps the CDEF index.
    cdef_records: [*mut MB_MODE_INFO; 4],
}

/// The block lists of a tile: what counts them, which is the tile's thread or, with
/// [`SinkOptions::pipe_lists`], whichever parse thread holds [`Self::claim`].
///
/// The entropy decoder of the tile writes the records and says how far it is ([`Self::ready`], at the
/// end of every superblock row); the thread that counts reads the records up to there, in order, and
/// writes the lists of the tile (its block map, index list and wavefront: `av1_mi_push_block`), which
/// the entropy decoder does not touch. New with it: 48 bytes a tile and two lists of the tile's pieces
/// (16 bytes a piece each, a piece being 256 records: 8 KB a tile for a heavy 4K key frame of 63k
/// blocks), besides the block context and the edges, which were the tile's before. The entropy decoder
/// and the thread that counts write different fields of the tile's `av1_tile_data` (the cursors of the
/// coefficients and transform block records; the lists'), and different parts of the host's memory.
struct TileLists {
    /// Held by the one thread that counts the tile's lists ([`Self::state`] is its while it holds it).
    claim: AtomicBool,
    /// [`Self::ready`] has records no thread has taken up to count: the tile is in
    /// [`Sink::queued_tiles`].
    queued: AtomicBool,
    /// How far the entropy decoder is through with the records: the piece in the high 32 bits, the
    /// records of that piece in the low ones.
    ready: AtomicU64,
    /// The pieces of the tile's records as the entropy decoder took them: the first record and how many
    /// there is room for. A piece before the one of [`Self::ready`] is full.
    pieces: Mutex<Vec<(*mut MB_MODE_INFO, c_int)>>,
    state: UnsafeCell<ListState>,
}

/// What the thread that counts a tile's lists keeps.
struct ListState {
    /// The block context `av1_mi_push_block` reads, cleared, with the tile's constants set. Its
    /// `tile_data` is [`Self::tile`], and `tile.tile_col` is the tile's index in the frame (the host's
    /// `index_list_room` finds the tile's index list by it).
    xd: Box<MACROBLOCKD>,
    /// With [`FrameState::pipe_lists`], the tile's `av1_tile_data` as the lists have it: a copy of the
    /// host's, taken when the tile begins, whose fields of the lists ([`lists_back`]) go back to the
    /// host's when the lists are counted. The entropy decoder writes the host's record at the same time
    /// (its cursors into the pools), so the thread that counts must not form a reference to that record:
    /// it has a record of its own. Without `pipe_lists` the lists are counted on the host's record (one
    /// thread does all of the tile), which is also what `rav1g-parse-check`'s reference, with one thread,
    /// holds this way against.
    tile: av1_tile_data,
    /// The records whose lists are counted: the piece, and the records of that piece.
    listed: (usize, c_int),
    /// The pieces of [`TileLists::pieces`] it has looked up.
    pieces: Vec<(*mut MB_MODE_INFO, c_int)>,
    /// The tile is timed (a host with a clock): the time stamps its block lists took, and with
    /// [`SinkOptions::time_calls`] the wavefront and intra part of them.
    timed: bool,
    list_stamps: u64,
    intra_stamps: u64,
    /// With [`SinkOptions::gpu_grid`], what the block lists read where the grid was: for every 4x4
    /// column of the tile the record of the lowest block whose lists are counted, and for every 4x4 row
    /// of a superblock row (by the row's number modulo [`MI_LEFT_EDGE`]) that of the rightmost. The
    /// blocks of a tile are counted in the order of the parse, so for the block that is counted next
    /// these are the blocks in the row above it and in the column to its left. `above` has a superblock
    /// more than the tile is wide, so that a block at the tile's right edge stays inside it.
    above: Vec<*mut MB_MODE_INFO>,
    left: [*mut MB_MODE_INFO; MI_LEFT_EDGE],
    /// The last 4x4 block counted in an even row and column: the first of the four 4x4 blocks of an 8x8,
    /// which the last of them (odd row and column) reads for its chroma.
    above_left: *mut MB_MODE_INFO,
}

impl TileLists {
    fn new() -> Self {
        Self {
            claim: AtomicBool::new(false),
            queued: AtomicBool::new(false),
            ready: AtomicU64::new(0),
            pieces: Mutex::new(Vec::new()),
            state: UnsafeCell::new(ListState {
                xd: zeroed_box(),
                // SAFETY: a C struct of numbers and pointers, for which all zero bytes are a value.
                tile: unsafe { std::mem::zeroed() },
                listed: (0, 0),
                pieces: Vec::new(),
                timed: false,
                list_stamps: 0,
                intra_stamps: 0,
                above: Vec::new(),
                left: [std::ptr::null_mut(); MI_LEFT_EDGE],
                above_left: std::ptr::null_mut(),
            }),
        }
    }

    /// Takes the claim if no thread holds it.
    fn try_claim(&self) -> bool {
        self.claim.compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed).is_ok()
    }

    fn release(&self) {
        self.claim.store(false, Ordering::Release);
    }
}

/// [`TileLists::ready`] of a tile whose entropy decoder has written these pieces.
fn ready_of(pieces: &[Records]) -> u64 {
    pieces.last().map_or(0, |records| ((pieces.len() as u64 - 1) << 32) | records.count as u32 as u64)
}

/// The fields of a tile's `av1_tile_data` that the lists write (`av1_mi_push_block` and what it calls:
/// the index list, the wavefront's iterations and the clears of their lists, intra block copy's
/// superblock, the single-edge schedule, whether the tile has inter blocks), from the lists' copy
/// ([`ListState::tile`]) into the host's record.
///
/// # Safety
/// `host` is the tile's record, which no other thread uses; the lists are through.
unsafe fn lists_back(host: *mut av1_tile_data, lists: &av1_tile_data) {
    // SAFETY: the caller's contract.
    let host = unsafe { &mut *host };
    host.gen_indexes = lists.gen_indexes;
    host.gen_index_ptr = lists.gen_index_ptr;
    host.gen_intra_iter_y = lists.gen_intra_iter_y;
    host.gen_intra_iter_uv = lists.gen_intra_iter_uv;
    host.gen_intra_iter_set = lists.gen_intra_iter_set;
    host.gen_iter_clear_offset = lists.gen_iter_clear_offset;
    host.gen_iter_clear_size = lists.gen_iter_clear_size;
    host.intra_iter_max = lists.intra_iter_max;
    host.intra_iter_max_uv = lists.intra_iter_max_uv;
    host.intrabc_sb = lists.intrabc_sb;
    host.intra_single_edge = lists.intra_single_edge;
    host.have_inter |= lists.have_inter;
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
    /// The block lists of the tiles. Written (the vector) by `frame_begin` and given back by `frame_end`
    /// under the write lock; the threads that look for lists to count ([`Sink::idle_work`]) read it.
    lists: RwLock<Vec<TileLists>>,
}

/// The sink.
pub struct Sink<H: SinkHost> {
    host: H,
    /// One place for every frame that may be in parse at once.
    frames: Box<[FramePlace]>,
    /// The frames that have begun.
    begun: AtomicU64,
    options: SinkOptions,
    /// The tiles of the frames in parse with records whose lists no thread has taken up to count
    /// ([`TileLists::queued`]).
    queued_tiles: AtomicUsize,
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
                lists: RwLock::new(Vec::new()),
            })
            .collect();
        Self { host, frames, begun: AtomicU64::new(0), options, queued_tiles: AtomicUsize::new(0) }
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

    /// The end of the call of the tile `key` that began at `begin`.
    ///
    /// # Safety
    /// As [`Self::tile_of`].
    #[inline]
    unsafe fn call_end(&self, key: usize, begin: u64) {
        if begin != 0 {
            // SAFETY: the caller's contract.
            unsafe { Self::tile_of(key) }.call_stamps += time_stamp().wrapping_sub(begin);
        }
    }

    /// The state of a tile from the number its calls come with: what its `tile_begin` returned, the
    /// address of the state.
    ///
    /// # Safety
    /// A call of the tile's thread between its `tile_begin` and the end of its frame, with what that
    /// `tile_begin` returned: the state is in the frame's place, whose vector of tiles is not written
    /// while tiles of the frame are in parse, and it is this thread's.
    #[inline(always)]
    unsafe fn tile_of<'a>(key: usize) -> &'a mut TileState {
        // SAFETY: the caller's contract.
        unsafe { &mut *(key as *mut TileState) }
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
    /// Counts the block lists of the records of a tile that are not counted yet, up to `ready` (as
    /// [`TileLists::ready`] has it), in the order they were read.
    ///
    /// # Safety
    /// Inside the frame, on the thread that holds the tile's claim, or on the tile's thread when the
    /// frame does not have [`FrameState::pipe_lists`]; the records up to `ready` are written. The block
    /// context is set for each block as `set_offsets_for_pred_and_recon` and `set_mi_row_col` of the
    /// entropy decoder set it; `av1_mi_push_block` reads the size of the frame through `cm`, which no
    /// thread writes while tiles of the frame are in parse. With [`SinkOptions::gpu_grid`] the block
    /// context has the block's record and the tile's edges in place of the grid, and each block that is
    /// counted goes into the edges.
    unsafe fn count_lists(&self, frame_number: usize, lists: &TileLists, ready: u64) {
        // The loop twice, so that the one with the grid is the code it was before there was another way.
        // SAFETY: the caller's contract.
        unsafe {
            if self.options.gpu_grid {
                self.count_lists_way::<true>(frame_number, lists, ready);
            } else {
                self.count_lists_way::<false>(frame_number, lists, ready);
            }
        }
    }

    /// [`Self::count_lists`]. `GPU_GRID`: [`SinkOptions::gpu_grid`].
    ///
    /// # Safety
    /// As for [`Self::count_lists`].
    unsafe fn count_lists_way<const GPU_GRID: bool>(&self, frame_number: usize, lists: &TileLists, ready: u64) {
        // SAFETY: the caller's contract; the records are those the tile has written into its parts of
        // the pool, and each is in the grid.
        unsafe {
            let state = &mut *lists.state.get();
            let (ready_piece, ready_count) = ((ready >> 32) as usize, ready as u32 as c_int);
            let (mut piece, mut index) = state.listed;
            // Nothing to count: the count stopped where the records end.
            if (piece, index) >= (ready_piece, ready_count) {
                return;
            }
            let frame = self.frame(frame_number);
            let stamp = if state.timed { time_stamp() } else { 0 };
            let time_intra = stamp != 0 && self.options.time_calls;
            if time_intra {
                rav1g_intra_list_stamps(true);
            }
            // The function finds the frame's slot through the tile's data and does not look at the
            // decoder.
            let pbi: *mut AV1Decoder = std::ptr::null_mut();
            let cm: *mut AV1_COMMON = &raw mut **self.frames[frame_number].cm.get();
            let xd: *mut MACROBLOCKD = &raw mut *state.xd;
            let (row_start, col_start) = ((*xd).tile.mi_row_start, (*xd).tile.mi_col_start);
            // The records piece by piece, in the order they were written. A piece before the one of
            // `ready` is full.
            loop {
                if (piece, index) >= (ready_piece, ready_count) {
                    break;
                }
                if piece >= state.pieces.len() {
                    let shared = lists.pieces.lock().unwrap_or_else(PoisonError::into_inner);
                    state.pieces.extend_from_slice(&shared[state.pieces.len()..]);
                }
                let (pool, max) = state.pieces[piece];
                let end = if piece < ready_piece { max } else { ready_count };
                if index >= end {
                    piece += 1;
                    index = 0;
                    continue;
                }
                let record = pool.add(index as usize);
                let mi = &*record;
                index += 1;
                let (mi_row, mi_col) = (i32::from(mi.mi_row), i32::from(mi.mi_col));
                let bw = 1 << MI_SIZE_WIDE_LOG2[mi.sb_type as usize];
                let bh = 1 << MI_SIZE_HIGH_LOG2[mi.sb_type as usize];
                if GPU_GRID {
                    (*xd).mi = std::ptr::null_mut();
                    (*xd).mi_record = record;
                    (*xd).mi_above = state.above.as_ptr().add((mi_col - col_start) as usize);
                    (*xd).mi_left = state.left.as_ptr();
                    (*xd).mi_above_left = state.above_left;
                } else {
                    (*xd).mi = frame.mi_grid.add((mi_row * frame.mi_stride + mi_col) as usize);
                    (*xd).mi_chunks = frame.mi_chunks;
                }
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
                if GPU_GRID {
                    // The block is now the lowest of its columns and the rightmost of its rows. Plain
                    // stores, a block's width and height of them (with the checks and the division of
                    // slices they were twice the time): the columns are inside `above`, which has a
                    // superblock more than the tile is wide, and a block lies at a multiple of its
                    // height, so its rows do not pass the end of `left`.
                    debug_assert!((mi_col - col_start + bw) as usize <= state.above.len());
                    debug_assert!(mi_row as usize % MI_LEFT_EDGE + bh as usize <= MI_LEFT_EDGE);
                    let above = state.above.as_mut_ptr().add((mi_col - col_start) as usize);
                    for column in 0..bw as usize {
                        above.add(column).write(record);
                    }
                    let left = state.left.as_mut_ptr().add(mi_row as usize % MI_LEFT_EDGE);
                    for row in 0..bh as usize {
                        left.add(row).write(record);
                    }
                    if bw == 1 && bh == 1 && (mi_row | mi_col) & 1 == 0 {
                        state.above_left = record;
                    }
                }
            }
            state.listed = (piece, index);
            if stamp != 0 {
                state.list_stamps += time_stamp().wrapping_sub(stamp);
            }
            if time_intra {
                state.intra_stamps += rav1g_intra_list_stamps(false);
            }
        }
    }

    /// With [`FrameState::pipe_lists`]: the claim of a tile's lists is taken, and the tile leaves
    /// [`Self::queued_tiles`]; its lists are counted up to where its entropy decoder is, and the claim is
    /// given back. `false` when another thread holds the claim.
    ///
    /// # Safety
    /// Inside the frame, which holds the tile's lists.
    unsafe fn count_claimed(&self, frame_number: usize, lists: &TileLists) -> bool {
        if !lists.try_claim() {
            return false;
        }
        if lists.queued.swap(false, Ordering::SeqCst) {
            self.queued_tiles.fetch_sub(1, Ordering::SeqCst);
        }
        // SAFETY: the caller's contract, and the claim is this thread's.
        unsafe { self.count_lists(frame_number, lists, lists.ready.load(Ordering::Acquire)) };
        lists.release();
        true
    }

    /// With [`FrameState::pipe_lists`]: the entropy decoder of a tile is through with a superblock row.
    /// The records up to here are the lists'; a parse thread without a task takes them up. When the
    /// row before is still there (no thread took it up a row long), the tile's thread counts both.
    ///
    /// # Safety
    /// The tile's thread, inside the frame.
    unsafe fn publish_row(&self, frame_number: usize, state: &TileState) {
        // SAFETY: the caller's contract; the lists are the tile's, in its frame's place.
        unsafe {
            let lists = &*state.lists;
            lists.ready.store(ready_of(&state.pieces), Ordering::Release);
            if !lists.queued.swap(true, Ordering::SeqCst) {
                self.queued_tiles.fetch_add(1, Ordering::SeqCst);
            } else {
                self.count_claimed(frame_number, lists);
            }
        }
    }

    /// The timers of a tile, for a host with a clock: when the tile and its lists are through.
    ///
    /// # Safety
    /// The tile's thread, inside the frame, with no other thread counting the tile's lists.
    unsafe fn tile_timers(&self, frame: usize, tile: usize) {
        // SAFETY: the caller's contract.
        unsafe {
            let state = self.tile(frame, tile).as_mut().expect("the tile has begun");
            let lists = &*(*state.lists).state.get();
            if state.begin_ticks != 0 {
                let stamps = time_stamp().wrapping_sub(state.begin_stamp);
                let ticks = self.host.ticks() - state.begin_ticks;
                if stamps != 0 {
                    let ticks_a_stamp = ticks as f64 / stamps as f64;
                    if self.options.time_calls {
                        self.host.add_time(frame, SinkTimer::Calls, (state.call_stamps as f64 * ticks_a_stamp) as i64);
                        self.host.add_time(frame, SinkTimer::Grid, (state.grid_stamps as f64 * ticks_a_stamp) as i64);
                        self.host.add_time(
                            frame,
                            SinkTimer::ListsIntra,
                            (lists.intra_stamps as f64 * ticks_a_stamp) as i64,
                        );
                    }
                    self.host.add_time(frame, SinkTimer::Lists, (lists.list_stamps as f64 * ticks_a_stamp) as i64);
                }
            }
        }
    }

    /// The tile's piece of mode info records is full, or it has none: the next one from the host, or
    /// `None` when there is none (the tile fails).
    #[cold]
    #[inline(never)]
    fn next_mode_info_piece<'a>(
        &self,
        frame_number: usize,
        tile: usize,
        state: &'a mut TileState,
    ) -> Option<&'a mut Records> {
        let td = state.memory.tile_data;
        // SAFETY: the tile's data, which the host gave for the tile; a call of the tile's thread.
        unsafe { (*td).mi_count = state.pieces.last().map_or(0, |records| records.count) };
        match self.host.more_mode_info(frame_number, tile, td) {
            Some(piece) if piece.count > 0 => {
                state.pieces.push(Records { pool: piece.mi_pool, count: 0, max: piece.count });
                // SAFETY: the tile's lists, in its frame's place, set at the tile's begin.
                unsafe { &*state.lists }
                    .pieces
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .push((piece.mi_pool, piece.count));
                state.pieces.last_mut()
            }
            _ => {
                state.failed = true;
                None
            }
        }
    }

    /// # Safety
    /// As [`Self::tile_of`].
    unsafe fn block_call(&self, frame_number: usize, key: usize, block: &ParseBlock<'_>) {
        // SAFETY: a call of the tile's thread between the frame's begin and end; the record is the next
        // one of the tile's part of the pool, and the grid is written inside the tile.
        unsafe {
            let state = Self::tile_of(key);
            let frame = &*state.frame;
            let tile = state.index;
            if state.failed {
                return;
            }
            // The tile's piece of the pool, and when it is full the next one (`decode_mbmi_block`,
            // `set_offsets`).
            let records = match state.pieces.last_mut() {
                Some(records) if records.count < records.max => records,
                _ => match self.next_mode_info_piece(frame_number, tile, state) {
                    Some(records) => records,
                    None => return,
                },
            };
            let mi = records.pool.add(records.count as usize);
            records.count += 1;
            let count = records.count;
            let lossless = frame.hdr.segmentation.lossless[block.b.seg_id.get()];
            state.lossless = lossless;
            let record_frame = RecordFrame {
                hdr: &frame.hdr,
                mi_cols: frame.mi_cols,
                mi_rows: frame.mi_rows,
                has_chroma: frame.has_chroma,
            };
            // The record cleared in the pool, then the fields of the block's kind. (A record put together
            // by itself and written to the pool once is the same bytes and was slower: every field has
            // to be stored then, and the lines of a record go to memory once either way;
            // docs/phase2-rav1d.md, "Records written once, and the lists of CDEF on the GPU".)
            mi.write_bytes(0, 1);
            record::fill(&mut *mi, &record_frame, block, lossless);

            let bsize = BLOCK_SIZE_OF[block.bs as usize] as usize;
            if self.options.gpu_grid {
                // No grid. The 64x64 areas whose top left unit is the block's: the block's first unit and,
                // of a block wider or higher than 64, every 64th from it. (A block lies at a multiple of
                // its width and of its height, so it has the top left unit of an area only this way.)
                if frame.cdef_coded && (block.x4 | block.y4) & 15 == 0 {
                    for y4 in (block.y4..block.y4 + (1 << MI_SIZE_HIGH_LOG2[bsize])).step_by(16) {
                        for x4 in (block.x4..block.x4 + (1 << MI_SIZE_WIDE_LOG2[bsize])).step_by(16) {
                            let area = if frame.sb128 { ((x4 >> 4) & 1) + ((y4 >> 4) & 1) * 2 } else { 0 };
                            state.cdef_records[area as usize] = mi;
                        }
                    }
                }
                return;
            }
            // The grid: the units of the block that are inside the frame.
            let x_mis = (1 << MI_SIZE_WIDE_LOG2[bsize]).min(frame.mi_cols - block.x4) as usize;
            let y_mis = (1 << MI_SIZE_HIGH_LOG2[bsize]).min(frame.mi_rows - block.y4) as usize;
            let first = frame.mi_grid.add((block.y4 * frame.mi_stride + block.x4) as usize);
            let first_record = (*state.memory.tile_data).mi_offset as usize;
            let entry = mi_grid_entry(first_record + count as usize - 1);
            let grid_begin = if self.options.time_calls { time_stamp() } else { 0 };
            for y in 0..y_mis {
                let row = first.add(y * frame.mi_stride as usize);
                for x in 0..x_mis {
                    row.add(x).write(entry);
                }
            }
            if grid_begin != 0 {
                state.grid_stamps += time_stamp().wrapping_sub(grid_begin);
            }
        }
    }

    /// # Safety
    /// As [`Self::tile_of`].
    unsafe fn txb_call(&self, frame_number: usize, key: usize, txb: &ParseTxb) -> *mut i32 {
        // SAFETY: a call of the tile's thread between the frame's begin and end; the record and the
        // coefficients are inside the tile's parts of their buffers (checked against their ends).
        unsafe {
            let state = Self::tile_of(key);
            let tile = state.index;
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

    /// # Safety
    /// As [`Self::tile_of`].
    unsafe fn superblock_call(&self, frame_number: usize, key: usize, superblock: &ParseSuperblock<'_>) {
        // SAFETY: a call of the tile's thread between the frame's begin and end; the grid is read inside
        // the tile, whose blocks have their records.
        let tile = unsafe {
            let state = Self::tile_of(key);
            let frame = &*state.frame;
            if frame.cdef_coded && self.options.gpu_grid {
                // The same without the grid: the block calls of the superblock kept the records.
                {
                    let areas = if frame.sb128 { 0..4 } else { 0..1 };
                    for area in areas {
                        let (x4, y4, index) = if frame.sb128 {
                            (superblock.x4 + (area & 1) * 16, superblock.y4 + (area >> 1) * 16, area as usize)
                        } else {
                            let index = ((superblock.x4 >> 4) & 1) + ((superblock.y4 >> 4) & 1) * 2;
                            (superblock.x4, superblock.y4, index as usize)
                        };
                        if x4 >= frame.mi_cols || y4 >= frame.mi_rows {
                            continue;
                        }
                        let mi = state.cdef_records[area as usize];
                        if !mi.is_null() {
                            (*mi).cdef_strength = superblock.cdef_idx[index];
                        }
                    }
                    state.cdef_records = [std::ptr::null_mut(); 4];
                }
            } else if frame.cdef_coded && !state.failed {
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
                    let mi = mi_grid_record_in_chunks(
                        frame.mi_chunks,
                        *frame.mi_grid.add((y4 * frame.mi_stride + x4) as usize),
                    );
                    if !mi.is_null() {
                        (*mi).cdef_strength = superblock.cdef_idx[index];
                    }
                }
            }
            state.index
        };
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
            cm.allow_intrabc = c_int::from(header.frame_hdr.allow_intrabc);
            *place.frame.get() = Some(FrameState {
                hdr: header.frame_hdr.clone(),
                mi_cols: header.bw,
                mi_rows: header.bh,
                mi_stride: memory.mi_stride,
                mi_grid: memory.mi_grid,
                mi_chunks: memory.mi_chunks,
                has_chroma,
                ss_x,
                ss_y,
                sb128: header.seq_hdr.sb128 != 0,
                cdef_coded: !(header.frame_hdr.all_lossless || header.frame_hdr.allow_intrabc),
                panics: self.options.test_panic && self.begun.fetch_add(1, Ordering::Relaxed) + 1 == TEST_PANIC_FRAME,
                pipe_lists: self.options.pipe_lists && header.tile_tasks,
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
            // The lists of the tiles, under the write lock: a thread that looks for lists to count may be
            // reading the vector (it finds none of this place's tiles queued).
            let mut lists = place.lists.write().unwrap_or_else(PoisonError::into_inner);
            for tile in lists.iter() {
                debug_assert!(!tile.queued.load(Ordering::Relaxed));
                tile.claim.store(false, Ordering::Relaxed);
                tile.ready.store(0, Ordering::Relaxed);
            }
            while lists.len() < tiles {
                lists.push(TileLists::new());
            }
        }
        Ok(number)
    }

    fn tile_begin(&self, frame_number: usize, tile: &ParseTile) -> usize {
        let memory = self.host.tile_begin(frame_number, tile);
        // SAFETY: a call of the tile's thread between the frame's begin and end.
        unsafe {
            let frame = self.frame(frame_number);
            let slot = self.tile(frame_number, tile.index);
            let state = slot.get_or_insert_with(|| TileState {
                index: tile.index,
                frame: std::ptr::null(),
                lists: std::ptr::null(),
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
                ended: false,
                finished: false,
                col_end: 0,
                sb_size: 0,
                call_stamps: 0,
                grid_stamps: 0,
                begin_ticks: 0,
                begin_stamp: 0,
                cdef_records: [std::ptr::null_mut(); 4],
            });
            // The tile's lists: the vector of the place is not written while tiles of the frame are in
            // parse, and no thread takes up the lists before the first superblock row is published below
            // this (`TileLists::queued`), so their state is this thread's here.
            let lists = &(*self.frames[frame_number].lists.read().unwrap_or_else(PoisonError::into_inner))[tile.index]
                as *const TileLists;
            state.lists = lists;
            let lists = &*lists;
            let list_state = &mut *lists.state.get();
            // A host with a clock (a timing run): what a time stamp is worth is taken over the tile.
            state.call_stamps = 0;
            state.grid_stamps = 0;
            state.begin_ticks = self.host.ticks();
            state.begin_stamp = if state.begin_ticks != 0 { time_stamp() } else { 0 };
            list_state.timed = state.begin_ticks != 0;
            list_state.list_stamps = 0;
            list_state.intra_stamps = 0;
            list_state.listed = (0, 0);
            list_state.pieces.clear();
            state.pieces.clear();
            {
                let mut pieces = lists.pieces.lock().unwrap_or_else(PoisonError::into_inner);
                pieces.clear();
                if memory.mi_count_max > 0 {
                    state.pieces.push(Records { pool: memory.mi_pool, count: 0, max: memory.mi_count_max });
                    pieces.push((memory.mi_pool, memory.mi_count_max));
                }
            }
            lists.ready.store(0, Ordering::Relaxed);
            state.txb_room = memory.txb_room;
            state.failed = false;
            state.ended = false;
            state.finished = false;
            state.col_end = tile.col_end;
            state.sb_size = if frame.sb128 { 32 } else { 16 };
            if self.options.gpu_grid {
                // The edges of the tile: nothing is read of them before a block of this tile has written
                // it (a block reads above and to the left only inside the tile).
                let columns = (tile.col_end - tile.col_start).max(0) as usize + MI_LEFT_EDGE + 1;
                list_state.above.clear();
                list_state.above.resize(columns, std::ptr::null_mut());
                list_state.left = [std::ptr::null_mut(); MI_LEFT_EDGE];
                list_state.above_left = std::ptr::null_mut();
            }
            state.cdef_records = [std::ptr::null_mut(); 4];
            let xd = &mut *list_state.xd;
            xd.tile = TileInfo {
                mi_row_start: tile.row_start,
                mi_row_end: tile.row_end,
                mi_col_start: tile.col_start,
                mi_col_end: tile.col_end,
                tile_row: 0,
                tile_col: tile.index as c_int,
            };
            // With `pipe_lists` the lists have a copy of the tile's record, which the host has set up and
            // the lines above have finished; nothing writes the host's record's fields of the lists from
            // here on. Otherwise the tile's thread counts them on the host's record, as no other thread
            // does anything of the tile.
            if frame.pipe_lists {
                list_state.tile = *memory.tile_data;
                xd.tile_data = &raw mut list_state.tile;
            } else {
                xd.tile_data = memory.tile_data;
            }
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
            state.index = tile.index;
            state.frame = frame;
            std::ptr::from_mut(state) as usize
        }
    }

    // The calls of a tile's blocks come with what its `tile_begin` returned (`key`), the address of
    // its state (`Self::tile_of`), on the tile's thread between that call and the end of the frame
    // (the contract of `ParseSink`).
    fn block(&self, frame: usize, key: usize, block: &ParseBlock<'_>) {
        let begin = self.call_begin();
        // SAFETY: see above.
        unsafe {
            self.block_call(frame, key, block);
            self.call_end(key, begin);
        }
    }

    fn txb(&self, frame: usize, key: usize, txb: &ParseTxb) -> *mut i32 {
        let begin = self.call_begin();
        // SAFETY: as above.
        unsafe {
            let coefs = self.txb_call(frame, key, txb);
            self.call_end(key, begin);
            coefs
        }
    }

    fn palette(&self, frame: usize, key: usize, palette: &ParsePalette<'_>) {
        let begin = self.call_begin();
        // SAFETY: as above.
        unsafe {
            self.host.palette(frame, Self::tile_of(key).index, palette);
            self.call_end(key, begin);
        }
    }

    fn superblock(&self, frame: usize, key: usize, superblock: &ParseSuperblock<'_>) {
        let begin = self.call_begin();
        // SAFETY: as above.
        unsafe {
            self.superblock_call(frame, key, superblock);
            self.call_end(key, begin);
        }
        // The block lists of the superblock, while its records are at hand; with `pipe_lists` those of
        // the superblock row at its end, by another thread. They have a timer of their own, so they are
        // not inside the timed call.
        // SAFETY: as above.
        let state = unsafe { Self::tile_of(key) };
        // SAFETY: as above; the frame's state is the frame's from its begin to its end.
        let frame_state = unsafe { &*state.frame };
        // SAFETY: as above.
        unsafe {
            if frame_state.pipe_lists {
                if superblock.x4 + state.sb_size >= state.col_end && !state.failed {
                    self.publish_row(frame, state);
                }
            } else {
                self.count_lists(frame, &*state.lists, ready_of(&state.pieces));
            }
        }
        if frame_state.panics {
            panic!("the test of a panic in the parse of a frame (SinkOptions::test_panic)");
        }
    }

    fn tile_end(&self, frame: usize, tile: usize) {
        // SAFETY: a call of the tile's thread between the frame's begin and end.
        unsafe {
            let pipe_lists = self.frame(frame).pipe_lists;
            let state = self.tile(frame, tile).as_mut().expect("the tile has begun");
            // The records of the tile's last piece; the pieces before it are full.
            let td = state.memory.tile_data;
            (*td).mi_count = state.pieces.last().map_or(0, |records| records.count);
            state.ended = true;
            if pipe_lists {
                // The rest of the lists, the timers and the host's end of the tile come with
                // `tile_finish`, behind the save of the frame's CDFs.
                return;
            }
            // The block lists that are not counted yet: none (the tile's last superblock counted them), and
            // the call is cheap then.
            self.count_lists(frame, &*state.lists, ready_of(&state.pieces));
            self.tile_timers(frame, tile);
        }
        self.host.tile_end(frame, tile);
    }

    fn tile_finish(&self, frame: usize, tile: usize) {
        // SAFETY: the thread of the tile's last superblock row, inside the frame (the contract of
        // `ParseSink::tile_finish`); the frame's place holds the tile's lists until its `frame_end`.
        unsafe {
            if !self.frame(frame).pipe_lists {
                return;
            }
            let Some(state) = self.tile(frame, tile).as_mut() else {
                // The tile did not begin: no lists.
                return;
            };
            if std::mem::replace(&mut state.finished, true) {
                return;
            }
            let lists = &*state.lists;
            // The thread that counts a row of the tile now is through with it in a row's time at most.
            let mut spins = 0u32;
            while !lists.try_claim() {
                spins += 1;
                if spins < 64 {
                    std::hint::spin_loop();
                } else {
                    std::thread::yield_now();
                }
            }
            if lists.queued.swap(false, Ordering::SeqCst) {
                self.queued_tiles.fetch_sub(1, Ordering::SeqCst);
            }
            let ended = state.ended && !state.failed;
            if ended {
                self.count_lists(frame, lists, ready_of(&state.pieces));
                // The claim is this thread's, and the tile's entropy decoder (this thread) is through:
                // the lists' fields go back to the host's record, which the host reads from here on.
                lists_back(state.memory.tile_data, &(*lists.state.get()).tile);
            }
            // Held from here on: no thread counts the tile's lists any more in this frame.
            if ended {
                self.tile_timers(frame, tile);
                self.host.tile_end(frame, tile);
            }
        }
    }

    fn idle_work_pending(&self) -> bool {
        self.queued_tiles.load(Ordering::Relaxed) != 0
    }

    fn idle_work(&self) -> bool {
        if self.queued_tiles.load(Ordering::Relaxed) == 0 {
            return false;
        }
        for (number, place) in self.frames.iter().enumerate() {
            // A frame that begins or ends has the lock: its tiles have no lists to count then.
            let Ok(lists) = place.lists.try_read() else {
                continue;
            };
            for tile in lists.iter() {
                // SAFETY: a tile with records to count is one of a frame in parse (it leaves the queue at
                // its `tile_finish`, before the frame ends), and the read lock keeps its frame's place.
                if tile.queued.load(Ordering::Relaxed) && unsafe { self.count_claimed(number, tile) } {
                    return true;
                }
            }
        }
        false
    }

    fn frame_end(&self, frame: usize, ok: bool) -> bool {
        let place = &self.frames[frame];
        // No thread counts lists of the frame any more (each tile's `tile_finish` took its claim for
        // good); one that looks for lists may still read the vector. The lock waits for it, and the place
        // is then the frame's alone to give back.
        drop(place.lists.write().unwrap_or_else(PoisonError::into_inner));
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
