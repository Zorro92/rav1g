// SPDX-License-Identifier: BSD-2-Clause

//! The buffers of a frame in plain memory, in the layout the D3D12 host gives them in a pipeline slot.
//!
//! Each of the two parsers has a [`Side`]. For every frame the side gets an [`Arena`]: the mode info pool,
//! the grid, the coefficients, the transform block records, the block index list, the block maps, the
//! wavefront grids of intra prediction and the tiles' cursors. A tile's parts of them are where
//! `av1_setup_macroblockd` of the host (`crates/rav1g/src/host/frame.rs`) puts them, with two choices that
//! take the host's second share out of the picture: a tile's part of the mode info pool has room for all
//! its blocks, and the coefficients are laid out for the worst case from the start (as the host does for a
//! frame with very small tiles).
//!
//! All of an arena starts as zero bytes, except the coefficients (a pattern, so that a coefficient one
//! parser writes and the other leaves out is seen) and the wavefront grids (-1, as the host clears them).
//! The parsed frame is the arena: the two sides' arenas of a frame are compared whole.

use std::alloc::{Layout, alloc_zeroed, dealloc, handle_alloc_error};
use std::ffi::c_int;
use std::sync::Mutex;

use crate::host::types::{Av1Core, av1_frame_thread_data};
use crate::sys::parser::{MB_MODE_INFO, av1_tile_data, tx_block_info_gpu};

/// Coefficients a 4x4 unit of the frame can have: 16 of luma and two times 4 of chroma
/// (`IDCT_COEF_COUNT_DENUM` of the host).
pub const COEFS_PER_UNIT: c_int = 4 * 4 + 2 * 2 * 2;
/// What the coefficient buffer holds before a parser writes it.
pub const COEF_PATTERN: i32 = 0x5A5A_5A5A;

/// Zeroed memory for `len` values of a C struct or a number.
pub struct Buffer<T> {
    ptr: *mut T,
    len: usize,
}

impl<T> Buffer<T> {
    pub fn new(len: usize) -> Self {
        let layout = Layout::array::<T>(len.max(1)).expect("the size of a buffer");
        // SAFETY: the layout has a size; the values kept in these buffers are C structs, numbers and
        // pointers, for which zero bytes are a value.
        let ptr = unsafe { alloc_zeroed(layout) }.cast::<T>();
        if ptr.is_null() {
            handle_alloc_error(layout);
        }
        Self { ptr, len }
    }

    pub fn ptr(&self) -> *mut T {
        self.ptr
    }

    pub fn len(&self) -> usize {
        self.len
    }

    /// The buffer as bytes.
    ///
    /// # Safety
    /// Nothing writes the buffer while the slice lives.
    pub unsafe fn bytes(&self) -> &[u8] {
        // SAFETY: the allocation, which was zeroed and so is initialised; the caller's contract.
        unsafe { std::slice::from_raw_parts(self.ptr.cast::<u8>(), self.len * size_of::<T>()) }
    }

    /// The buffer as values.
    ///
    /// # Safety
    /// Nothing writes the buffer while the slice lives.
    pub unsafe fn values(&self) -> &[T] {
        // SAFETY: as for `bytes`.
        unsafe { std::slice::from_raw_parts(self.ptr, self.len) }
    }
}

impl<T> Drop for Buffer<T> {
    fn drop(&mut self) {
        let layout = Layout::array::<T>(self.len.max(1)).expect("the size of a buffer");
        // SAFETY: the allocation of `new` with the same layout.
        unsafe { dealloc(self.ptr.cast(), layout) };
    }
}

// SAFETY: a buffer is memory; who writes which part of it is the business of the code that holds it.
unsafe impl<T> Send for Buffer<T> {}
// SAFETY: as above.
unsafe impl<T> Sync for Buffer<T> {}

/// The size of a frame as both parsers know it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct FrameSize {
    /// The frame in 4x4 units.
    pub mi_cols: c_int,
    pub mi_rows: c_int,
    pub tile_cols: c_int,
    pub tile_rows: c_int,
    /// The frame has chroma planes.
    pub has_chroma: bool,
}

impl FrameSize {
    /// `calc_mi_size` of `av1/common/onyxc_int.h`: the distance of the grid's rows.
    pub fn mi_stride(&self) -> c_int {
        (self.mi_cols + 31) & !31
    }

    pub fn tile_count(&self) -> c_int {
        self.tile_cols * self.tile_rows
    }
}

/// A tile in 4x4 units.
#[derive(Clone, Copy)]
pub struct TileBounds {
    pub index: usize,
    pub row_start: c_int,
    pub row_end: c_int,
    pub col_start: c_int,
    pub col_end: c_int,
}

/// A palette block's plane as the host gets it (`av1_intra_palette`): the colours and one colour index
/// per sample of the block, the part outside the frame filled in.
#[derive(PartialEq, Eq, Debug)]
pub struct Palette {
    pub mi_row: c_int,
    pub mi_col: c_int,
    /// 0: luma, 1: both chroma planes.
    pub plane: u8,
    pub colors: Vec<u16>,
    pub width: usize,
    pub height: usize,
    pub map: Vec<u8>,
}

/// A loop restoration unit a parser has read: plane, index of the unit in the plane, type
/// (`RestorationType` of libaom) and the coefficients of that type.
#[derive(PartialEq, Eq, Debug, Clone, Copy, PartialOrd, Ord)]
pub struct LrUnit {
    pub plane: u8,
    pub unit_x: u16,
    pub unit_y: u16,
    pub r#type: u8,
    /// Wiener: the three coded taps of the vertical and of the horizontal filter. SGR: set and weights.
    pub values: [i32; 6],
}

/// The buffers of one frame.
pub struct Arena {
    pub size: FrameSize,
    pub mi_pool: Buffer<MB_MODE_INFO>,
    pub mi_grid: Buffer<*mut MB_MODE_INFO>,
    pub coefs: Buffer<i32>,
    pub tx_blocks: Buffer<tx_block_info_gpu>,
    pub gen_indexes: Buffer<u32>,
    pub block_map: Buffer<c_int>,
    /// Size of the block maps without the slack behind them (`pred_map_size` of the host).
    pub pred_map_size: c_int,
    /// The wavefront grids: luma, and chroma behind it.
    pub iter_grid: Buffer<c_int>,
    pub iter_grid_stride: c_int,
    pub iter_grid_offset_uv: c_int,
    pub tiles: Buffer<av1_tile_data>,
    /// The palette blocks of each tile in the order they were read.
    pub palettes: Vec<Mutex<Vec<Palette>>>,
    /// The loop restoration units that were read.
    pub lr_units: Mutex<Vec<LrUnit>>,
    /// A block with global motion is warped from this reference (`gm_warp` of the host's slot).
    pub gm_warp: [c_int; 8],
    /// The parser finished the frame without an error.
    pub ok: bool,
}

impl Arena {
    pub fn new(size: FrameSize) -> Self {
        let cols16 = (size.mi_cols + 15) & !15;
        let rows16 = (size.mi_rows + 15) & !15;
        let units = (cols16 * rows16) as usize;
        let stride = size.mi_stride();
        let tiles = size.tile_count() as usize;
        let grid_w = (((size.mi_cols * 4 + 63) & !63) >> 2) + 2 + 128;
        let grid_h = (((size.mi_rows * 4 + 63) & !63) >> 2) + 2 + 128;
        let pred_map_size = (256 + 32) * size.tile_count() + (units as c_int >> 2) * 10 + 1024;
        let arena = Self {
            size,
            mi_pool: Buffer::new((stride * ((size.mi_rows + 31) & !31)) as usize + 64),
            mi_grid: Buffer::new((stride * ((size.mi_rows + 31) & !31)) as usize),
            coefs: Buffer::new(units * COEFS_PER_UNIT as usize + 64),
            tx_blocks: Buffer::new((units * 3).div_ceil(2) + 64),
            gen_indexes: Buffer::new(units * 3 + 64),
            block_map: Buffer::new(pred_map_size as usize + 4096),
            pred_map_size,
            iter_grid: Buffer::new((grid_w * grid_h) as usize * 3 / 2 + 64),
            iter_grid_stride: grid_w,
            iter_grid_offset_uv: grid_w * grid_h,
            tiles: Buffer::new(tiles),
            palettes: (0..tiles).map(|_| Mutex::new(Vec::new())).collect(),
            lr_units: Mutex::new(Vec::new()),
            gm_warp: [0; 8],
            ok: false,
        };
        arena.fill();
        arena
    }

    /// What the coefficients and the wavefront grids hold before a parser writes them.
    fn fill(&self) {
        // SAFETY: the two buffers, which nothing else works on.
        unsafe {
            for i in 0..self.coefs.len() {
                self.coefs.ptr().add(i).write(COEF_PATTERN);
            }
            self.iter_grid.ptr().cast::<u8>().write_bytes(0xFF, self.iter_grid.len() * size_of::<c_int>());
        }
    }

    /// The arena as `new` leaves it, for another frame of the same size: the timing run uses one arena
    /// for all frames, so that the parser writes memory the process already has.
    pub fn reset(&mut self, gm_warp: [c_int; 8]) {
        // SAFETY: the buffers of the arena, which nothing else works on.
        unsafe {
            self.mi_pool.ptr().write_bytes(0, self.mi_pool.len());
            self.mi_grid.ptr().write_bytes(0, self.mi_grid.len());
            self.tx_blocks.ptr().write_bytes(0, self.tx_blocks.len());
            self.gen_indexes.ptr().write_bytes(0, self.gen_indexes.len());
            self.block_map.ptr().write_bytes(0, self.block_map.len());
            self.tiles.ptr().write_bytes(0, self.tiles.len());
        }
        self.fill();
        for palettes in &self.palettes {
            palettes.lock().expect("no thread panics with it").clear();
        }
        self.lr_units.lock().expect("no thread panics with it").clear();
        self.gm_warp = gm_warp;
        self.ok = false;
    }

    /// The slot of the frame for `av1_mi_push_block`.
    pub fn frame_data(&self) -> av1_frame_thread_data {
        av1_frame_thread_data {
            gm_warp: self.gm_warp,
            gen_intra_iter_y: self.iter_grid.ptr(),
            // SAFETY: the chroma grid lies behind the luma grid in the buffer.
            gen_intra_iter_uv: unsafe { self.iter_grid.ptr().add(self.iter_grid_offset_uv as usize) },
            iter_grid_stride: self.iter_grid_stride,
            iter_grid_stride_uv: self.iter_grid_stride >> 1,
            intra_v2: 0,
            intra_v2_sched: Default::default(),
            scale_factors: Default::default(),
            scale_enable: 0,
        }
    }

    /// A tile's cursors into the buffers, as `av1_setup_macroblockd` of the host sets them. Returns the
    /// tile's data, its part of the mode info pool and the records that part has room for.
    ///
    /// # Safety
    /// One thread at a time works on a tile; the tile is one of the frame.
    pub unsafe fn setup_tile(&self, tile: &TileBounds) -> (*mut av1_tile_data, *mut MB_MODE_INFO, c_int) {
        assert!(tile.index < self.tiles.len(), "a tile of the frame");
        let size = &self.size;
        let rows = tile.row_end - tile.row_start;
        let cols = tile.col_end - tile.col_start;
        let tile_rows = (rows + 15) & !15;
        let tile_cols = (cols + 15) & !15;
        let mi_offset = tile.row_start * ((size.mi_cols + 15) & !15) + tile.col_start * tile_rows;
        let mi_max = tile_rows * tile_cols;
        let tile_id = tile.index as c_int;
        // SAFETY: the tile's entry; the parts of the buffers it points into lie inside them (the sizes of
        // `new` are those of the host for a decoder of this frame size, and the asserts below hold them).
        unsafe {
            let t_ptr = self.tiles.ptr().add(tile.index);
            t_ptr.write_bytes(0, 1);
            let t = &mut *t_ptr;
            t.dq_buffer_offset = mi_offset * COEFS_PER_UNIT;
            t.dq_buffer_ptr = t.dq_buffer_offset;
            t.dq_buffer_base = self.coefs.ptr();
            t.dq_buffer_size = mi_max * COEFS_PER_UNIT;
            t.dq_buffer_end = t.dq_buffer_ptr + t.dq_buffer_size;
            // The host switches to its second share at this mark; here it is never looked at.
            t.dq_buffer_max = t.dq_buffer_end;
            assert!(t.dq_buffer_end as usize <= self.coefs.len());

            let blocks_4x4_count = (mi_offset * 3 + 1) >> 1;
            t.intra_iter_max = -1;
            t.intra_iter_max_uv = -1;
            t.blocks_offset = blocks_4x4_count;
            t.idct_blocks_host = self.tx_blocks.ptr().add(blocks_4x4_count as usize);
            assert!((blocks_4x4_count + (mi_max * 3 + 1) / 2) as usize <= self.tx_blocks.len());

            let mi_offset_base = tile.row_start * size.mi_stride() + tile.col_start * rows;
            let mi_count_max = rows * cols;
            t.mi_offset = mi_offset_base;
            t.mi_count = 0;
            assert!((mi_offset_base + mi_count_max) as usize <= self.mi_pool.len());

            t.mi_col_start = tile.col_start;
            t.mi_row_start = tile.row_start;
            t.gen_index_ptr = 0;
            t.gen_index_base = blocks_4x4_count * 2;
            t.gen_indexes = self.gen_indexes.ptr().add(t.gen_index_base as usize);
            t.gen_block_warp_offset = tile_id * (256 + 32) + (mi_offset >> 2) * 10;
            t.gen_block_map_offset = t.gen_block_warp_offset + 32;
            t.gen_block_map = self.block_map.ptr().add(t.gen_block_map_offset as usize);
            t.gen_block_map_wrp = self.block_map.ptr().add(t.gen_block_warp_offset as usize);
            t.gen_intra_iter_y = -1;
            t.gen_intra_iter_uv = -1;
            t.have_inter = 0;
            t.gen_intra_max_iter = if size.tile_count() == 1 {
                (self.pred_map_size - 512) / 10
            } else {
                ((tile_cols * tile_rows) >> 2) - 2
            };
            t.gen_intra_iter_set = (t.gen_intra_max_iter - 16).min(256);
            t.gen_iter_clear_offset = (t.gen_intra_iter_set + 16) * 10 + 256;
            t.gen_iter_clear_size =
                (size_of::<c_int>() as c_int).wrapping_mul(256 + t.gen_intra_max_iter * 10 - t.gen_iter_clear_offset);
            (t_ptr, self.mi_pool.ptr().add(mi_offset_base as usize), mi_count_max)
        }
    }
}

/// One of the two parsers: the decoder `av1_mi_push_block` finds its frame through, the frame in parse
/// and the frames that are parsed.
pub struct Side {
    /// What `gpu_decoder` of an `AV1Decoder` points to.
    pub core: Av1Core,
    pub frame_data: av1_frame_thread_data,
    /// The frame in parse.
    pub arena: Option<Arena>,
    /// The parsed frames in decode order, until they are compared.
    pub frames: Vec<Arena>,
    /// Frames whose parse ended with an error.
    pub failed: usize,
    /// The timing run: the frames are not kept; the arena of one is the next one's.
    pub recycle: bool,
    spare: Option<Arena>,
    /// Frames parsed in the timing run.
    pub recycled: usize,
    /// What only the host of the entropy decoder of libaom keeps.
    pub c: crate::chost::CState,
}

impl Side {
    pub fn new() -> Box<Self> {
        let mut side = Box::new(Self {
            core: Av1Core { curr_frame_data: std::ptr::null_mut(), side: std::ptr::null_mut() },
            frame_data: Arena::new(FrameSize {
                mi_cols: 16,
                mi_rows: 16,
                tile_cols: 1,
                tile_rows: 1,
                has_chroma: true,
            })
            .frame_data(),
            arena: None,
            frames: Vec::new(),
            failed: 0,
            recycle: false,
            spare: None,
            recycled: 0,
            c: Default::default(),
        });
        side.frame_data.gen_intra_iter_y = std::ptr::null_mut();
        side.frame_data.gen_intra_iter_uv = std::ptr::null_mut();
        side.core.curr_frame_data = &raw mut side.frame_data;
        side.core.side = &raw mut *side;
        side
    }

    /// A frame's parse starts: its arena.
    pub fn begin_frame(&mut self, size: FrameSize, gm_warp: [c_int; 8]) -> &Arena {
        let arena = match self.spare.take() {
            Some(mut arena) if arena.size == size => {
                arena.reset(gm_warp);
                arena
            }
            _ => {
                let mut arena = Arena::new(size);
                arena.gm_warp = gm_warp;
                arena
            }
        };
        self.frame_data = arena.frame_data();
        self.arena.insert(arena)
    }

    /// The frame's parse has ended.
    pub fn end_frame(&mut self, ok: bool) {
        if let Some(mut arena) = self.arena.take() {
            if self.recycle {
                self.recycled += usize::from(ok);
                self.spare = Some(arena);
            } else if ok {
                arena.ok = true;
                self.frames.push(arena);
            } else {
                // A frame that failed is not a parsed frame: its buffers are dropped.
                self.failed += 1;
            }
        }
        self.frame_data.gen_intra_iter_y = std::ptr::null_mut();
        self.frame_data.gen_intra_iter_uv = std::ptr::null_mut();
    }
}
