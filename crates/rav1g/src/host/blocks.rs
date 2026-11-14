// SPDX-License-Identifier: BSD-2-Clause
//
// This source code is subject to the terms of the BSD 2 Clause License and the Alliance for Open Media
// Patent License 1.0. If the BSD 2 Clause License was not distributed with this source code in the LICENSE
// file, you can obtain it at www.aomedia.org/license/software. If the Alliance for Open Media Patent
// License 1.0 was not distributed with this source code in the PATENTS file, you can obtain it at
// www.aomedia.org/license/patent.

//! The block lists of the prediction stages: for every block the entropy decoder has decoded, its place
//! in the lists of blocks that `gen_pred_blocks.hlsl` fills and the prediction stages run over. From
//! `av1_mi_push_block` of `libav1/dx/prediction.cpp`.
//!
//! The tile threads call this once per mode info block. It counts the prediction blocks the block needs
//! into the lists of the tile (`av1_tile_data::gen_block_map`: inter blocks by type and size, intra
//! blocks by wavefront iteration and size, reconstruction blocks by size) and writes the index each of
//! them has in its list behind the block's index base (`gen_indexes`); the shader that writes the block
//! records finds them there. For intra blocks it also keeps the wavefront: a transform block takes the
//! highest iteration of the neighbours it reads, plus one.
//!
//! Where the iterations of the neighbours are kept (docs/phase2-rav1d.md, "No clears by the CPU"):
//!
//! - **In the edges of the tile** ([`WaveEdges`], the default): the iteration at the lowest decoded unit
//!   of every column, at the rightmost decoded unit of every row of the superblock row, and at the unit
//!   above that one, with where the decoded units end. A transform block reads units along the row above
//!   it and the column to its left, which are those, and the frame's memory is not touched for them: of
//!   the slot's wavefront grid only the one entry per transform block that `gen_pred_blocks.hlsl` reads
//!   is written, so the grid needs no clear. Neither do the edges: an entry that is read is one of this
//!   frame.
//! - **In the slot's wavefront grid** (`av1_tile_data::wave_edges` null): an entry per 4x4 unit, written
//!   along the right and the bottom edge of every transform block and read back by the blocks next to
//!   it. Every other unit has to read as "no iteration", so the grid is cleared for every frame.
//!
//! Both give every transform block the same iteration.

use std::ffi::c_int;

use super::intra_v2;
use super::tables::{
    INTER_BLOCK_SIZE_INDEX_LUT, INTERINTRA_TO_INTRA_MODE, MAX_NEIGHBOR_OBMC, MAX_TXSIZE_RECT_LOOKUP, MI_SIZE_HIGH_LOG2,
    MI_SIZE_WIDE_LOG2, SS_SIZE_LOOKUP, TX_SIZE_HIGH_LOG2, TX_SIZE_WIDE_LOG2,
};
use super::types::{
    CompoundAvrg, CompoundDiff, CompoundDiffUv, CompoundDiffUvGlobalWarp, CompoundGlobalWarp, CompoundMasked,
    Inter2x2ArrOffset, InterFastCompound, InterSizesAllCommon, IntraBlockOffset, IntraSizes, IntraTypeCount, ObmcAbove,
    ObmcLeft, ReconBlockOffset, av1_frame_thread_data, av1_perf_inter_tile,
};
use crate::sys::parser::{
    AV1_COMMON, AV1_PERF_INTER_CHROMA_2X2, AV1_PERF_INTER_COMP_AVG, AV1_PERF_INTER_COMP_AVG_FAST,
    AV1_PERF_INTER_COMP_DIFF_UV, AV1_PERF_INTER_COMP_DIFF_Y, AV1_PERF_INTER_COMP_MASKED, AV1_PERF_INTER_COMP_WARP,
    AV1_PERF_INTER_COMP_WARP_DIFF_UV, AV1_PERF_INTER_OBMC_ABOVE, AV1_PERF_INTER_OBMC_LEFT, AV1_PERF_INTER_SINGLE,
    AV1_PERF_INTER_SINGLE_COPY, AV1_PERF_INTER_WARP, AV1Decoder, BILINEAR, BLOCK_4X4, BLOCK_4X8, BLOCK_4X16, BLOCK_8X4,
    BLOCK_8X8, BLOCK_8X16, BLOCK_16X4, BLOCK_16X8, BLOCK_32X32, COMPOUND_DIFFWTD, COMPOUND_WEDGE, GLOBAL_GLOBALMV,
    GLOBALMV, INTRA_FRAME, MACROBLOCKD, MB_MODE_INFO, OBMC_CAUSAL, REF_NO_SCALE, SCALE_SUBPEL_BITS,
    SINGLE_INTER_MODE_END, SINGLE_INTER_MODE_START, TX_4X4, TX_16X32, TX_16X64, TX_32X16, TX_32X32, TX_32X64, TX_64X16,
    TX_64X32, TX_64X64, UV_CFL_PRED, WARPED_CAUSAL, av1_tile_data, int_mv,
};

unsafe extern "C" {
    /// `av1/common/reconintra.c`: the samples above and to the right of a transform block are decoded.
    fn has_top_right(
        cm: *const AV1_COMMON,
        bsize: u8,
        mi_row: c_int,
        mi_col: c_int,
        top_available: c_int,
        right_available: c_int,
        partition: u8,
        txsz: u8,
        row_off: c_int,
        col_off: c_int,
        ss_x: c_int,
        ss_y: c_int,
    ) -> c_int;
    /// `av1/common/reconintra.c`: the same for the samples to the left and below.
    fn has_bottom_left(
        cm: *const AV1_COMMON,
        bsize: u8,
        mi_row: c_int,
        mi_col: c_int,
        bottom_available: c_int,
        left_available: c_int,
        partition: u8,
        txsz: u8,
        row_off: c_int,
        col_off: c_int,
        ss_x: c_int,
        ss_y: c_int,
    ) -> c_int;
}

// What an intra mode reads of its neighbours, one bit per mode. testing/intra_check reads these five lines
// (`const NEED_..._LUT: u32 = 0x...;`) for its model of the host.
const NEED_ABOVE_LUT: u32 = 0x3f7f; // 11 1111 0111 1?11
const NEED_LEFT_LUT: u32 = 0x3ef7; // 11 1110 1111 01?1
const NEED_RIGHT_LUT: u32 = 0x010a; // 0 0001 0000 10?0
const NEED_BOT_LUT: u32 = 0x0084; // 0 0000 1000 0?00
const NEED_ABOVE_LEFT_LUT: u32 = 0x11ff; // 1 0001 1111 111?  - set to 1 for DC mode to workaround Filter mode

const INTRA: i8 = INTRA_FRAME as i8;
const SIZES: c_int = InterSizesAllCommon as c_int;

/// `is_interintra_pred` of `av1/common/blockd.h`: an inter block with an intra prediction blended in.
#[inline]
pub(crate) fn is_interintra_pred(sb_type: u8, mode: u8, ref_frame: [i8; 2]) -> bool {
    ref_frame[0] > INTRA
        && ref_frame[1] == INTRA
        && (BLOCK_8X8 as u8..=BLOCK_32X32 as u8).contains(&sb_type)
        && (SINGLE_INTER_MODE_START as u8..SINGLE_INTER_MODE_END as u8).contains(&mode)
}

/// `scale_chroma_bsize` of `av1/common/onyxc_int.h` for planes of half the width and height: the luma
/// block size whose chroma block a block under 8x8 belongs to.
#[inline]
pub(crate) fn scale_chroma_bsize_420(bsize: u8) -> u8 {
    const B4X4: u8 = BLOCK_4X4 as u8;
    const B4X8: u8 = BLOCK_4X8 as u8;
    const B8X4: u8 = BLOCK_8X4 as u8;
    const B4X16: u8 = BLOCK_4X16 as u8;
    const B16X4: u8 = BLOCK_16X4 as u8;
    match bsize {
        B4X4 | B4X8 | B8X4 => BLOCK_8X8 as u8,
        B4X16 => BLOCK_8X16 as u8,
        B16X4 => BLOCK_16X8 as u8,
        other => other,
    }
}

/// `av1_get_max_uv_txsize` of `av1/common/blockd.h`: the transform size of the chroma planes of a block.
#[inline]
pub(crate) fn max_uv_txsize(bsize: u8, subsampling_x: c_int, subsampling_y: c_int) -> u8 {
    const T64X64: u8 = TX_64X64 as u8;
    const T64X32: u8 = TX_64X32 as u8;
    const T32X64: u8 = TX_32X64 as u8;
    const T64X16: u8 = TX_64X16 as u8;
    const T16X64: u8 = TX_16X64 as u8;
    let plane_bsize = SS_SIZE_LOOKUP[(bsize as usize * 4) + (subsampling_x as usize * 2) + subsampling_y as usize];
    // av1_get_adjusted_tx_size: no chroma transform is 64 wide or high.
    match MAX_TXSIZE_RECT_LOOKUP[plane_bsize as usize] {
        T64X64 | T64X32 | T32X64 => TX_32X32 as u8,
        T64X16 => TX_32X16 as u8,
        T16X64 => TX_16X32 as u8,
        other => other,
    }
}

/// `av1_extract_interp_filter` of `av1/common/filter.h`: the filter of one direction.
#[inline]
pub(crate) fn extract_interp_filter(filters: u32, x_filter: bool) -> u32 {
    (filters >> if x_filter { 16 } else { 0 }) & 0xf
}

// ---------------------------------------------------------------------------------------------------------
// Inter breakdown of the timing run (-gputimedetail, dx/av1_perf.h): the work counters of the tile threads.
// Nothing here runs unless the tile has counters (av1_tile_data::perf_inter).
// ---------------------------------------------------------------------------------------------------------

/// A reference of another size than the frame.
fn perf_ref_scaled(td: &av1_frame_thread_data, ref_frame: i8) -> bool {
    let sf = &td.scale_factors[ref_frame as usize];
    td.scale_enable != 0
        && sf.x_step > 0
        && sf.y_step > 0
        && (sf.x_scale != REF_NO_SCALE as c_int || sf.y_scale != REF_NO_SCALE as c_int)
}

/// One prediction block of `section`: `w` x `h` samples of the luma plane, or of each chroma plane
/// (`uv`), from the first `nrefs` references of `mi` (a neighbour's mode info for an overlapped
/// prediction and for the chroma of a block under 8x8). `warp`: the block is warped, so the fraction of
/// the vector says nothing.
#[expect(clippy::too_many_arguments, reason = "the arguments of the C++ function")]
fn perf_inter_count(
    pc: &mut av1_perf_inter_tile,
    td: &av1_frame_thread_data,
    section: c_int,
    uv: bool,
    w: c_int,
    h: c_int,
    nrefs: usize,
    mi: &MB_MODE_INFO,
    warp: bool,
) {
    let k = &mut pc.work[section as usize];
    let planes = if uv { 2 } else { 1 };
    if uv {
        k.blocks_uv += 1;
        k.pixels_uv += i64::from(2 * w * h);
    } else {
        k.blocks_y += 1;
        k.pixels_y += i64::from(w * h);
    }
    k.ref_blocks += nrefs as i64 * planes;
    let frac_mask = if uv { 15 } else { 7 }; // the vector is in 1/8 luma samples: 1/16 chroma samples
    for r in 0..nrefs {
        let mut fw = w + 7;
        let mut fh = h + 7;
        let mut mw = fw;
        let mut mh = fh;
        if perf_ref_scaled(td, mi.ref_frame[r]) {
            let sf = &td.scale_factors[mi.ref_frame[r] as usize];
            fw = (((w - 1) * sf.x_step) >> SCALE_SUBPEL_BITS) + 8;
            fh = (((h - 1) * sf.y_step) >> SCALE_SUBPEL_BITS) + 8;
            mw = fw;
            mh = fh;
        } else if !warp {
            // SAFETY: a motion vector is two 16-bit numbers in either form of the union.
            let mv = unsafe { mi.mv[r].as_mv };
            if c_int::from(mv.col) & frac_mask == 0 {
                mw = w;
            }
            if c_int::from(mv.row) & frac_mask == 0 {
                mh = h;
            }
        }
        k.footprint += planes * i64::from(fw * fh);
        k.footprint_mv += planes * i64::from(mw * mh);
    }
}

/// One inter block (a mode info block) by what it uses, in luma samples.
#[expect(clippy::too_many_arguments, reason = "the arguments of the C++ function")]
fn perf_inter_count_block(
    pc: &mut av1_perf_inter_tile,
    td: &av1_frame_thread_data,
    mi: &MB_MODE_INFO,
    bw_log: c_int,
    bh_log: c_int,
    is_compound: bool,
    is_obmc: bool,
    is_warp: bool,
    is_inter_intra: bool,
) {
    let b = &mut pc.blocks;
    let area = 16i64 << (bw_log + bh_log);
    b.blocks += 1;
    b.area += area;
    if is_compound {
        b.area_compound += area;
    }
    if is_obmc {
        b.area_obmc += area;
    }
    if is_warp {
        b.area_warp += area;
    }
    if is_compound && mi.interinter_comp.type_ == COMPOUND_WEDGE as u32 {
        b.area_wedge += area;
    }
    if is_compound && mi.interinter_comp.type_ == COMPOUND_DIFFWTD as u32 {
        b.area_diffwtd += area;
    }
    if is_inter_intra {
        b.area_interintra += area;
    }
    let mut scaled = false;
    for r in 0..1 + usize::from(is_compound) {
        let ref_scaled = perf_ref_scaled(td, mi.ref_frame[r]);
        scaled |= ref_scaled;
        b.ref_area += area;
        // SAFETY: a motion vector is two 16-bit numbers in either form of the union.
        let mv = unsafe { mi.mv[r].as_mv };
        let frac =
            if is_warp || ref_scaled { 3 } else { usize::from(mv.col & 7 != 0) | (usize::from(mv.row & 7 != 0) << 1) };
        b.ref_area_mv[frac] += area;
        // The kernels the block asks for (av1_get_interp_filter_params_with_block_size): the 4-tap form of
        // the filter for a side of 4.
        let fx = extract_interp_filter(mi.interp_filters, true);
        let fy = extract_interp_filter(mi.interp_filters, false);
        let kernel = |filter: u32, side_log: c_int| {
            if filter == BILINEAR as u32 {
                2
            } else if side_log == 0 {
                1
            } else {
                0
            }
        };
        b.ref_area_filter[0][kernel(fx, bw_log)] += area;
        b.ref_area_filter[1][kernel(fy, bh_log)] += area;
    }
    if scaled {
        b.area_scaled += area;
    }
}

/// -interfast: the class of a motion vector by its fraction (`InterFastCopy..InterFast2D` of
/// `dx/types.h`), as gen_pred_blocks_fast.hlsl takes it from the block record. `frac_mask`: 7 for luma,
/// whose vector is in 1/8 sample, 15 for chroma (1/16 chroma sample).
#[inline]
fn inter_fast_class(mv: &int_mv, frac_mask: i16) -> c_int {
    // SAFETY: a motion vector is two 16-bit numbers in either form of the union.
    let mv = unsafe { mv.as_mv };
    c_int::from(mv.col & frac_mask != 0) | (c_int::from(mv.row & frac_mask != 0) << 1)
}

/// The value of a list counter, which then counts `by` more: the index of the next block of the list.
///
/// # Safety
/// `list` is a counter of the tile's block map.
#[inline(always)]
unsafe fn take(list: *mut c_int, by: c_int) -> c_int {
    // SAFETY: the caller's contract.
    unsafe {
        let index = *list;
        *list = index + by;
        index
    }
}

/// The rows of 4x4 units whose edge a tile keeps: those of a superblock of 128 samples. A transform block
/// reads the column to its left down to the end of its superblock at most.
const WAVE_ROWS: usize = 32;
const WAVE_ROW_MASK: usize = WAVE_ROWS - 1;

/// The value of a unit no intra transform block of the frame has written: the block there is predicted
/// before the wavefront starts (an inter block, a palette block).
const NO_ITERATION: c_int = -1;

/// The edges of a tile's intra wavefront in one plane: what the wavefront grid holds at the units a
/// transform block can still read, which are the ones along the border of what the tile has decoded.
///
/// With `G(c, r)` for the iteration of the transform block at the unit in column `c` and row `r` of the
/// plane ([`NO_ITERATION`] where the block is none of the wavefront, and where nothing is decoded yet,
/// as in a grid that was cleared for the frame), and the blocks of a tile decoded in an order in which a
/// block comes behind every block above it and every block to its left:
///
/// - `above[c]` is `G` at the lowest decoded unit of column `c`, and `above_end[c]` the row below that
///   unit;
/// - `rows[r & 31]` has, for the rows of the superblock row that is being decoded, `G` at the rightmost
///   decoded unit of row `r` ([`LEFT`]) and `G` at the unit above that one ([`CORNER`]): the unit above
///   and to the left of the block that comes next in row `r`, if `r` is that block's first row;
///   `left_end[r & 31]` is the column to the right of the rightmost decoded unit.
///
/// Every block of the tile is entered when it is decoded, while the tile is not idle ([`WaveEdges`]): a
/// transform block with its iteration
/// ([`WavePlane::commit`]), a block without one as "no iteration" ([`WavePlane::pass`]). The units above
/// a block (over its width), to its left (over its height) and at its
/// corner are decoded whenever the block may read them, so their entries were written by the blocks
/// that lie there in this frame. The units further to the right in the row above, and further down in the
/// column to the left, are read on the word of `has_top_right` and `has_bottom_left`, which for an
/// inter-intra block are asked with the size of its residual transform and can name units that are not
/// decoded yet (a cleared grid has "no iteration" there). For those the ends say whether the unit is
/// the decoded one: the ends carry the stamp of the frame (`stamp`: the frame's number on its slot,
/// shifted left by 32), so an entry of an earlier frame never says so, and one of an earlier superblock
/// row or of a block further up names another row or column. So nothing has to be cleared between
/// frames.
#[repr(C)]
pub struct WavePlane {
    /// By the column of the plane in the frame; valid for the columns of the tile, and for those of a
    /// block that reaches past the right edge of the frame.
    pub above: *mut c_int,
    pub above_end: *mut i64,
    pub rows: [[c_int; 2]; WAVE_ROWS],
    pub left_end: [i64; WAVE_ROWS],
    pub stamp: i64,
}

/// In [`WavePlane::rows`]: the iteration at the rightmost decoded unit of the row.
const LEFT: usize = 0;
/// In [`WavePlane::rows`]: the iteration at the unit above that one.
const CORNER: usize = 1;

/// `count` numbers from `first` on become `value`. The counts of a block are powers of two; the small ones,
/// which most blocks have, are a store or two here and not a call.
///
/// # Safety
/// `first` is valid for writing `count` values.
#[inline(always)]
unsafe fn fill<T: Copy>(first: *mut T, count: usize, value: T) {
    // SAFETY: the caller's contract.
    unsafe {
        match count {
            1 => first.write(value),
            2 => first.cast::<[T; 2]>().write_unaligned([value; 2]),
            4 => first.cast::<[T; 4]>().write_unaligned([value; 4]),
            8 => first.cast::<[T; 8]>().write_unaligned([value; 8]),
            _ => {
                for at in 0..count {
                    first.add(at).write(value);
                }
            }
        }
    }
}

impl WavePlane {
    /// The iterations of a block of `w` x `h` units at `col`, `row` of the plane that is decoded: `iter` at
    /// the lowest unit of its columns and at the rightmost unit of its rows.
    ///
    /// # Safety
    /// `above` is valid for the columns of the block.
    #[inline(always)]
    unsafe fn enter(&mut self, col: c_int, row: c_int, w: c_int, h: c_int, iter: c_int) {
        let (w, h) = (w as usize, h as usize);
        let first = row as usize & WAVE_ROW_MASK;
        // SAFETY: the caller's contract, and the rows of the block when they do not wrap.
        unsafe {
            let above = self.above.offset(col as isize);
            // What was above the block's right column is above and to the left of the block that follows
            // in the block's first row; in its other rows that unit is one of the block's own.
            let above_right = *above.add(w - 1);
            fill(above, w, iter);
            if first + h <= WAVE_ROWS {
                // A block lies at a multiple of its height: its rows do not wrap.
                fill(self.rows.as_mut_ptr().add(first), h, [iter; 2]);
            } else {
                for it in 0..h {
                    self.rows[(first + it) & WAVE_ROW_MASK] = [iter; 2];
                }
            }
            self.rows[first][CORNER] = above_right;
        }
    }

    /// A transform block of `w` x `h` units at `col`, `row` of the plane is decoded, with its iteration:
    /// the iterations, and where the decoded units of its columns and rows end now.
    ///
    /// # Safety
    /// `above` and `above_end` are valid for the columns of the block.
    #[inline(always)]
    unsafe fn commit(&mut self, col: c_int, row: c_int, w: c_int, h: c_int, iter: c_int) {
        let below = self.stamp | i64::from(row + h);
        let right = self.stamp | i64::from(col + w);
        let (first, h) = (row as usize & WAVE_ROW_MASK, h as usize);
        // SAFETY: the caller's contract, and the rows of the block when they do not wrap.
        unsafe {
            self.enter(col, row, w, h as c_int, iter);
            fill(self.above_end.offset(col as isize), w as usize, below);
            if first + h <= WAVE_ROWS {
                fill(self.left_end.as_mut_ptr().add(first), h, right);
            } else {
                for it in 0..h {
                    self.left_end[(first + it) & WAVE_ROW_MASK] = right;
                }
            }
        }
    }

    /// A block of `w` x `h` units at `col`, `row` of the plane that is none of the wavefront is decoded
    /// (an inter block, a palette block): [`NO_ITERATION`] at its units. Where the decoded units end is
    /// left as it was: the ends are asked only about units that may not be decoded, which read as "no
    /// iteration" as well, and an end that an earlier transform block left names a unit further up or
    /// further left than the one that is asked for, or the entry it is asked about holds this block's
    /// "no iteration" by now.
    ///
    /// # Safety
    /// `above` is valid for the columns of the block.
    #[inline(always)]
    unsafe fn pass(&mut self, col: c_int, row: c_int, w: c_int, h: c_int) {
        // SAFETY: the caller's contract.
        unsafe { self.enter(col, row, w, h, NO_ITERATION) };
    }
}

/// The edges of a tile's intra wavefront (`av1_tile_data::wave_edges`): luma, then chroma.
///
/// **Idle.** Most blocks of most frames are inter blocks, far from any intra block, and entering their
/// "no iteration" at the edges is all the edges would cost them. So the edges are kept only while the
/// tile has transform blocks of the wavefront near: a tile starts idle, and it goes idle again at the
/// start of a superblock row when the superblock row before had none. A block reads units of its own
/// superblock row and of the one above, so while a tile is idle every unit a block can read is "no
/// iteration", and nothing is entered. The first intra block behind that wakes the tile: the edges are
/// made what they would be by then, "no iteration" in every column of the tile and in every row, and
/// are kept from there on.
#[repr(C)]
pub struct WaveEdges {
    pub planes: [WavePlane; 2],
    /// Not 0: the edges are not kept (see above); what they hold is not read.
    pub idle: c_int,
    /// The superblock row of the block the tile was at last, and the last one with a transform block of
    /// the wavefront; -1 and -2 at the start of the tile.
    pub sb_row: c_int,
    pub last_wave_sb_row: c_int,
    /// The columns of the tile in luma: the first, and how many.
    pub first_col: c_int,
    pub cols: c_int,
}

impl WaveEdges {
    /// A tile starts: idle. `first_col` and `cols` are its columns in 4x4 units of luma.
    pub fn start(&mut self, first_col: c_int, cols: c_int) {
        self.idle = 1;
        self.sb_row = -1;
        self.last_wave_sb_row = -2;
        self.first_col = first_col;
        self.cols = cols;
    }

    /// The tile is at a block of the superblock row `sb_row`: idle from the start of a superblock row
    /// whose row before had no transform block of the wavefront.
    #[inline(always)]
    fn at(&mut self, sb_row: c_int) {
        if sb_row != self.sb_row {
            self.sb_row = sb_row;
            if self.last_wave_sb_row < sb_row - 1 {
                self.idle = 1;
            }
        }
    }

    /// The tile is at an intra block: its edges are kept from here on. If it was idle, no block of this
    /// superblock row or of the one above has an iteration, which is what every entry a block can read
    /// then has to say.
    ///
    /// # Safety
    /// The rows above of the two planes are valid for the columns of the tile.
    #[inline(always)]
    unsafe fn wake(&mut self) {
        self.last_wave_sb_row = self.sb_row;
        if self.idle != 0 {
            // SAFETY: the caller's contract.
            unsafe { self.reset() };
        }
    }

    /// # Safety
    /// As for [`WaveEdges::wake`].
    #[cold]
    #[inline(never)]
    unsafe fn reset(&mut self) {
        self.idle = 0;
        for (plane, shift) in self.planes.iter_mut().zip([0, 1]) {
            let first = self.first_col >> shift;
            let end = (self.first_col + self.cols + shift) >> shift;
            // SAFETY: the caller's contract.
            unsafe {
                std::slice::from_raw_parts_mut(plane.above.offset(first as isize), (end - first) as usize)
                    .fill(NO_ITERATION);
            }
            plane.rows = [[NO_ITERATION; 2]; WAVE_ROWS];
        }
    }
}

/// What the intra part needs to know of a plane of the block.
struct IntraPlane {
    /// The wavefront grid of the plane at the block, and the distance of its rows.
    iter_grid: *mut c_int,
    iter_grid_stride: c_int,
    mode: u32,
    /// log2 of the transform block's width and height in 4x4 units of the plane.
    txw: c_int,
    txh: c_int,
    is_intrabc: bool,
}

impl IntraPlane {
    fn need(&self, lut: u32) -> bool {
        (lut >> self.mode) & 1 != 0
    }
}

/// `av1_mi_push_block` (`dx/av1_core.h`): counts the prediction blocks of the block `xd` is at into the
/// lists of its tile.
///
/// # Safety
/// Called by the entropy decoder on the thread that decodes the tile: `pbi` is the decoder, `cm` the
/// frame header, `xd` the thread's block context with the block's mode info and its tile's data.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn av1_mi_push_block(pbi: *mut AV1Decoder, cm: *mut AV1_COMMON, xd: *mut MACROBLOCKD) {
    // SAFETY: the caller's contract. The frame data is shared by the tile threads and only read here; the
    // tile data, the block's mode info and the parts of the wavefront grids the tile covers belong to
    // this thread. The mode info of the neighbours is read while the pointer to the block's own is not
    // written through.
    unsafe {
        let dec = (*pbi).gpu_decoder.cast::<super::types::Av1Core>();
        let td = &*(*dec).curr_frame_data.cast_const();
        let xd = &*xd.cast_const();
        let tile = &mut *xd.tile_data;
        let index_arr = tile.gen_indexes;
        let map = tile.gen_block_map;
        // The edges of the tile's wavefront; null when the frame keeps the wavefront in the slot's grid.
        let wave = tile.wave_edges.cast::<WaveEdges>();

        (**xd.mi).index_base = tile.gen_index_ptr;
        let mi = &**xd.mi;
        let neighbour = |offset: c_int| &**xd.mi.offset(offset as isize);
        let bsize = mi.sb_type;
        let bw_log = c_int::from(MI_SIZE_WIDE_LOG2[bsize as usize]);
        let bh_log = c_int::from(MI_SIZE_HIGH_LOG2[bsize as usize]);
        let bw = 1 << bw_log;
        let bh = 1 << bh_log;
        let bw_log_uv = (bw_log - 1).max(0);
        let bh_log_uv = (bh_log - 1).max(0);

        let mi_row = mi.mi_row;
        let mi_col = mi.mi_col;

        // The block has no chroma of its own: the block to its right or below carries it.
        let is_chroma_ref = ((mi_row & 1) == 0 && (bh & 1) == 1) || ((mi_col & 1) == 0 && (bw & 1) == 1);

        let is_inter = mi.ref_frame[0] > INTRA;
        let is_inter_intra = is_interintra_pred(mi.sb_type, mi.mode, mi.ref_frame);
        let is_compound = mi.ref_frame[1] > INTRA;
        let is_obmc = is_inter
            && u32::from(mi.motion_mode) == OBMC_CAUSAL as u32
            && (xd.up_available != 0 || xd.left_available != 0);
        // Inter breakdown of the timing run (-gputimedetail): the tile's counters, null without it.
        let mut pc = tile.perf_inter.cast::<av1_perf_inter_tile>().as_mut();
        // -interfast: the tile's lists by motion vector class, null on a frame that keeps the lists by type.
        let fast_map = tile.gen_block_map_fast;
        let mut push = |value: c_int| {
            *index_arr.offset(tile.gen_index_ptr as isize) = value as u32;
            tile.gen_index_ptr += 1;
        };
        if is_inter {
            tile.have_inter = 1;
            let mut is_global_warp = [false; 2];
            if xd.cur_frame_force_integer_mv == 0
                && bsize >= BLOCK_8X8 as u8
                && (u32::from(mi.mode) == GLOBALMV as u32 || u32::from(mi.mode) == GLOBAL_GLOBALMV as u32)
            {
                for reference in 0..1 + usize::from(is_compound) {
                    // As gen_pred_blocks.hlsl decides it from GenBlockData::global_warp: both count the block
                    // into the same list.
                    is_global_warp[reference] = td.gm_warp[mi.ref_frame[reference] as usize] != 0;
                }
            }
            let is_local_warp = xd.cur_frame_force_integer_mv == 0
                && u32::from(mi.motion_mode) == WARPED_CAUSAL as u32
                && mi.wm_params.invalid == 0;
            let is_luma_warp = (is_local_warp || is_global_warp[0]) && bw_log >= 1 && bh_log >= 1;
            let is_chroma_warp = is_luma_warp && bw_log >= 2 && bh_log >= 2;
            let comp_type = mi.interinter_comp.type_;

            // Y:
            let block_size_id_y = INTER_BLOCK_SIZE_INDEX_LUT[(bw_log * 6 + bh_log) as usize];
            if is_compound {
                let is_warp_compound = is_global_warp[0] || is_global_warp[1];
                let (list_type, mut section) = if is_warp_compound {
                    (CompoundGlobalWarp, AV1_PERF_INTER_COMP_WARP)
                } else if comp_type == COMPOUND_WEDGE as u32 {
                    (CompoundMasked, AV1_PERF_INTER_COMP_MASKED)
                } else if comp_type == COMPOUND_DIFFWTD as u32 {
                    (CompoundDiff, AV1_PERF_INTER_COMP_DIFF_Y)
                } else {
                    (CompoundAvrg, AV1_PERF_INTER_COMP_AVG)
                };
                let type_index = (list_type - 1) * SIZES + block_size_id_y;
                if !fast_map.is_null() && list_type == CompoundAvrg {
                    let cls = inter_fast_class(&mi.mv[0], 7) | (inter_fast_class(&mi.mv[1], 7) << 2);
                    push(take(fast_map.offset((InterFastCompound + cls * SIZES + block_size_id_y) as isize), 1));
                    section = AV1_PERF_INTER_COMP_AVG_FAST + cls;
                } else {
                    push(take(map.offset(type_index as isize), 1));
                }
                if let Some(pc) = pc.as_deref_mut() {
                    perf_inter_count(pc, td, section, false, 4 << bw_log, 4 << bh_log, 2, mi, is_warp_compound);
                }
            } else {
                let mut section = if is_luma_warp { AV1_PERF_INTER_WARP } else { AV1_PERF_INTER_SINGLE };
                if is_luma_warp {
                    push(take(tile.gen_block_map_wrp.offset(block_size_id_y as isize), 1));
                } else if !fast_map.is_null() {
                    let cls = inter_fast_class(&mi.mv[0], 7);
                    push(take(fast_map.offset((cls * SIZES + block_size_id_y) as isize), 1));
                    section = AV1_PERF_INTER_SINGLE_COPY + cls;
                } else {
                    push(take(map.offset(block_size_id_y as isize), 1));
                }
                if let Some(pc) = pc.as_deref_mut() {
                    perf_inter_count(pc, td, section, false, 4 << bw_log, 4 << bh_log, 1, mi, is_luma_warp);
                }
            }
            if let Some(pc) = pc.as_deref_mut() {
                perf_inter_count_block(
                    pc,
                    td,
                    mi,
                    bw_log,
                    bh_log,
                    is_compound,
                    is_obmc,
                    is_luma_warp || (is_compound && (is_global_warp[0] || is_global_warp[1])),
                    is_inter_intra,
                );
            }

            // UV:
            if !is_chroma_ref {
                let plane_bw = (bw_log - 1).max(0);
                let plane_bh = (bh_log - 1).max(0);
                let block_size_id_uv = INTER_BLOCK_SIZE_INDEX_LUT[(plane_bw * 6 + plane_bh) as usize];
                // A block under 8x8 predicts its chroma 2x2 samples at a time, each from the block it lies
                // under, when all of those are inter blocks.
                let mut sub8x8 = bw_log == 0 || bh_log == 0;
                if sub8x8 {
                    let dy = if bh_log == 0 { -1 } else { 0 };
                    let dx = if bw_log == 0 { -1 } else { 0 };
                    for offset in [dx + dy * xd.mi_stride, dx, dy * xd.mi_stride] {
                        sub8x8 &= neighbour(offset).ref_frame[0] > INTRA;
                    }
                }

                if sub8x8 {
                    let brows = if bh_log == 2 { 4 } else { 2 };
                    let bcols = if bw_log == 2 { 4 } else { 2 };
                    for row in 0..brows {
                        let mi_index = (if bh_log == 0 { row - 1 } else { 0 }) * xd.mi_stride;
                        let sub_mi = neighbour(mi_index);

                        if bw_log == 0 {
                            let sub_mi0 = neighbour(mi_index - 1);
                            let is_compound1 = c_int::from(sub_mi.ref_frame[1] > INTRA);
                            let is_compound0 = c_int::from(sub_mi0.ref_frame[1] > INTRA);
                            let differ = c_int::from(is_compound0 != is_compound1);
                            let type_index0 = Inter2x2ArrOffset + (is_compound0 << differ);
                            let type_index1 = Inter2x2ArrOffset + (is_compound1 << differ);
                            push(*map.offset(type_index0 as isize));
                            if type_index0 != type_index1 {
                                push(*map.offset(type_index1 as isize));
                            }
                            *map.offset(type_index0 as isize) += 2;
                            *map.offset(type_index1 as isize) += 2;
                            if let Some(pc) = pc.as_deref_mut() {
                                // Two 2x2 pieces in a row: the left one from the block to the left, the right
                                // one from this.
                                perf_inter_count(pc, td, AV1_PERF_INTER_CHROMA_2X2, true, 2, 2, 1, sub_mi0, false);
                                perf_inter_count(pc, td, AV1_PERF_INTER_CHROMA_2X2, true, 2, 2, 1, sub_mi, false);
                            }
                        } else {
                            let type_index = Inter2x2ArrOffset + c_int::from(is_compound);
                            push(take(map.offset(type_index as isize), 2 * bcols));
                            if let Some(pc) = pc.as_deref_mut() {
                                for _ in 0..bcols {
                                    perf_inter_count(pc, td, AV1_PERF_INTER_CHROMA_2X2, true, 2, 2, 1, sub_mi, false);
                                }
                            }
                        }
                    }
                } else if is_compound {
                    let is_warp_compound = (is_global_warp[0] || is_global_warp[1]) && bw_log >= 2 && bh_log >= 2;
                    let (list_type, mut section) = if is_warp_compound {
                        if comp_type == COMPOUND_DIFFWTD as u32 {
                            (CompoundDiffUvGlobalWarp, AV1_PERF_INTER_COMP_WARP_DIFF_UV)
                        } else {
                            (CompoundGlobalWarp, AV1_PERF_INTER_COMP_WARP)
                        }
                    } else if comp_type == COMPOUND_WEDGE as u32 {
                        (CompoundMasked, AV1_PERF_INTER_COMP_MASKED)
                    } else if comp_type == COMPOUND_DIFFWTD as u32 {
                        (CompoundDiffUv, AV1_PERF_INTER_COMP_DIFF_UV)
                    } else {
                        (CompoundAvrg, AV1_PERF_INTER_COMP_AVG)
                    };
                    let type_index = (list_type - 1) * SIZES + block_size_id_uv;
                    if !fast_map.is_null() && list_type == CompoundAvrg {
                        let cls = inter_fast_class(&mi.mv[0], 15) | (inter_fast_class(&mi.mv[1], 15) << 2);
                        push(take(fast_map.offset((InterFastCompound + cls * SIZES + block_size_id_uv) as isize), 2));
                        section = AV1_PERF_INTER_COMP_AVG_FAST + cls;
                    } else {
                        push(take(map.offset(type_index as isize), 2));
                    }
                    if let Some(pc) = pc.as_deref_mut() {
                        perf_inter_count(pc, td, section, true, 4 << plane_bw, 4 << plane_bh, 2, mi, is_warp_compound);
                    }
                } else {
                    let mut section = if is_chroma_warp { AV1_PERF_INTER_WARP } else { AV1_PERF_INTER_SINGLE };
                    if is_chroma_warp {
                        push(take(tile.gen_block_map_wrp.offset(block_size_id_uv as isize), 2));
                    } else if !fast_map.is_null() {
                        let cls = inter_fast_class(&mi.mv[0], 15);
                        push(take(fast_map.offset((cls * SIZES + block_size_id_uv) as isize), 2));
                        section = AV1_PERF_INTER_SINGLE_COPY + cls;
                    } else {
                        push(take(map.offset(block_size_id_uv as isize), 2));
                    }
                    if let Some(pc) = pc.as_deref_mut() {
                        perf_inter_count(pc, td, section, true, 4 << plane_bw, 4 << plane_bh, 1, mi, is_chroma_warp);
                    }
                }
            }

            if is_obmc {
                // The blocks above, then the blocks to the left, that the prediction is blended with: at most
                // max_neighbor_obmc of each, one prediction block per neighbour and plane.
                let mut count = 0;
                if xd.up_available != 0 {
                    let x_mis = bw.min((*cm).mi_cols - mi_col);
                    let h = if bh_log > 4 { 3 } else { bh_log - 1 };
                    let huv = if h == 0 { 0 } else { h - 1 };
                    let obmc_chroma =
                        bsize > BLOCK_16X8 as u8 && bsize != BLOCK_4X16 as u8 && bsize != BLOCK_16X4 as u8;
                    let mut col = 0;
                    while col < x_mis && count < MAX_NEIGHBOR_OBMC[bw_log as usize] {
                        let mut above = col - xd.mi_stride;
                        let mut w = c_int::from(MI_SIZE_WIDE_LOG2[neighbour(above).sb_type as usize]);
                        if w == 0 {
                            w = 1;
                            above += 1;
                        }
                        if w > bw_log {
                            w = bw_log;
                        }
                        let above_mi = neighbour(above);
                        if above_mi.ref_frame[0] > INTRA {
                            count += 1 + c_int::from(w == 5);
                            let type_index_y = (ObmcAbove - 1) * SIZES + ((w << 2) | h);
                            push(take(map.offset(type_index_y as isize), 1));
                            if let Some(pc) = pc.as_deref_mut() {
                                perf_inter_count(
                                    pc,
                                    td,
                                    AV1_PERF_INTER_OBMC_ABOVE,
                                    false,
                                    4 << w,
                                    4 << h,
                                    1,
                                    above_mi,
                                    false,
                                );
                            }
                            if obmc_chroma {
                                let type_index_uv = (ObmcAbove - 1) * SIZES + (((w - 1) << 2) | huv);
                                push(take(map.offset(type_index_uv as isize), 2));
                                if let Some(pc) = pc.as_deref_mut() {
                                    perf_inter_count(
                                        pc,
                                        td,
                                        AV1_PERF_INTER_OBMC_ABOVE,
                                        true,
                                        4 << (w - 1),
                                        4 << huv,
                                        1,
                                        above_mi,
                                        false,
                                    );
                                }
                            }
                        }

                        assert!(above_mi.use_intrabc == 0);
                        col += 1 << w;
                    }
                }
                count = 0;
                if xd.left_available != 0 {
                    let y_mis = bh.min((*cm).mi_rows - mi_row);
                    let w = if bw_log > 4 { 3 } else { bw_log - 1 };
                    let wuv = if w == 0 { 0 } else { w - 1 };
                    let mut row = 0;
                    while row < y_mis && count < MAX_NEIGHBOR_OBMC[bh_log as usize] {
                        let mut left = xd.mi_stride * row - 1;
                        let mut h = c_int::from(MI_SIZE_HIGH_LOG2[neighbour(left).sb_type as usize]);
                        if h == 0 {
                            h = 1;
                            left += xd.mi_stride;
                        }
                        if h > bh_log {
                            h = bh_log;
                        }
                        let left_mi = neighbour(left);
                        if left_mi.ref_frame[0] > INTRA {
                            count += 1 + c_int::from(h == 5);

                            let type_index_y = (ObmcLeft - 1) * SIZES + ((h << 2) | w);
                            let type_index_uv = (ObmcLeft - 1) * SIZES + (((h - 1) << 2) | wuv);
                            push(take(map.offset(type_index_y as isize), 1));
                            push(take(map.offset(type_index_uv as isize), 2));
                            if let Some(pc) = pc.as_deref_mut() {
                                perf_inter_count(
                                    pc,
                                    td,
                                    AV1_PERF_INTER_OBMC_LEFT,
                                    false,
                                    4 << w,
                                    4 << h,
                                    1,
                                    left_mi,
                                    false,
                                );
                                perf_inter_count(
                                    pc,
                                    td,
                                    AV1_PERF_INTER_OBMC_LEFT,
                                    true,
                                    4 << wuv,
                                    4 << (h - 1),
                                    1,
                                    left_mi,
                                    false,
                                );
                            }
                        }
                        assert!(left_mi.use_intrabc == 0);
                        row += 1 << h;
                    }
                }
            }
        }

        let y_use_palette = mi.palette_mode_info.palette_size[0] > 0;
        let uv_use_palette = mi.palette_mode_info.palette_size[1] > 0;
        if let Some(edges) = wave.as_mut() {
            edges.at(mi_row >> (*cm).seq_params.mib_size_log2);
            if !is_inter || is_inter_intra {
                // The block has, or may have, transform blocks of the wavefront.
                edges.wake();
            }
        }
        if !is_inter || is_inter_intra {
            let is_intrabc = mi.use_intrabc != 0;
            let bsize_uv = scale_chroma_bsize_420(bsize);
            // av1_get_tx_size: 4x4 for a lossless segment, else the block's transform size for luma and the
            // largest of the chroma block for chroma.
            let lossless = xd.lossless[mi.segment_id as usize] != 0;
            let tx_sizes = [
                if lossless { TX_4X4 as u8 } else { mi.tx_size },
                if lossless {
                    TX_4X4 as u8
                } else {
                    max_uv_txsize(mi.sb_type, xd.plane[1].subsampling_x, xd.plane[1].subsampling_y)
                },
            ];
            // The intra mode of the intra part of an inter-intra block.
            let interintra_mode =
                if is_inter_intra { u32::from(INTERINTRA_TO_INTRA_MODE[mi.interintra_mode as usize]) } else { 0 };
            // A block of 128 samples is predicted as units of 64.
            let unit_x_log = c_int::from(bw_log == 5 && !is_intrabc);
            let unit_y_log = c_int::from(bh_log == 5 && !is_intrabc);
            // With the edges: the highest iteration of the luma under the block's chroma, which a block
            // predicted from luma (CFL) waits for. The luma of the block itself is counted below. A block
            // under 8x8 carries the chroma of the blocks to its left and above it too: their luma is at
            // the edges now, before the block's own luma is entered there.
            let mut luma_under = NO_ITERATION;
            if let Some(edges) = wave.as_ref()
                && !is_chroma_ref
                && !uv_use_palette
                && !is_inter_intra
                && u32::from(mi.uv_mode) == UV_CFL_PRED as u32
                && (bw == 1 || bh == 1)
            {
                let luma = &edges.planes[0];
                // The luma units under the chroma transform block: twice its size, from the even unit.
                let (first_col, first_row) = (mi_col & !1, mi_row & !1);
                let end_col = first_col + (2 << (TX_SIZE_WIDE_LOG2[tx_sizes[1] as usize] - 2));
                let end_row = first_row + (2 << (TX_SIZE_HIGH_LOG2[tx_sizes[1] as usize] - 2));
                if mi_col > first_col {
                    for row in mi_row..end_row {
                        luma_under = luma_under.max(luma.rows[row as usize & WAVE_ROW_MASK][LEFT]);
                    }
                }
                if mi_row > first_row {
                    for col in mi_col..end_col {
                        luma_under = luma_under.max(*luma.above.offset(col as isize));
                    }
                }
                if mi_col > first_col && mi_row > first_row {
                    luma_under = luma_under.max(luma.rows[mi_row as usize & WAVE_ROW_MASK][CORNER]);
                }
            }
            if !y_use_palette {
                let txw = if is_intrabc {
                    bw_log.min(4)
                } else if is_inter_intra {
                    bw_log
                } else {
                    TX_SIZE_WIDE_LOG2[tx_sizes[0] as usize] - 2
                };
                let txh = if is_intrabc {
                    bh_log.min(4)
                } else if is_inter_intra {
                    bh_log
                } else {
                    TX_SIZE_HIGH_LOG2[tx_sizes[0] as usize] - 2
                };
                let max_cnt_x = ((*cm).mi_cols - mi_col + (1 << txw) - 1) >> txw;
                let max_cnt_y = ((*cm).mi_rows - mi_row + (1 << txh) - 1) >> txh;
                let cnt_y = (1 << (bh_log - txh)) >> unit_y_log;
                let cnt_x = (1 << (bw_log - txw)) >> unit_x_log;
                let iter_grid_stride = td.iter_grid_stride;
                let plane = IntraPlane {
                    iter_grid: td.gen_intra_iter_y.offset((mi_col + mi_row * iter_grid_stride) as isize),
                    iter_grid_stride,
                    mode: if is_inter_intra { interintra_mode } else { u32::from(mi.mode) },
                    txw,
                    txh,
                    is_intrabc,
                };

                let need_above = plane.need(NEED_ABOVE_LUT) && !is_intrabc;
                let need_left = plane.need(NEED_LEFT_LUT) && !is_intrabc;
                let need_right = plane.need(NEED_RIGHT_LUT);
                let need_bot = plane.need(NEED_BOT_LUT);
                let need_aboveleft = plane.need(NEED_ABOVE_LEFT_LUT) && !is_intrabc;
                let use_filter = mi.filter_intra_mode_info.use_filter_intra != 0;

                let type_size = txw + txh;
                let type_idx_base = if use_filter { IntraBlockOffset } else { IntraBlockOffset - 1 - type_size };
                // The size of a transform block in 4x4 units. (The C++ had to declare these outside its loops:
                // MSVC 14.51 at /O1 /Ot without /GL read subblk_w from a stack temporary it had not written
                // yet when the declaration was in the innermost loop; docs/rust-port.md has the listing.)
                let subblk_w = 1 << txw;
                let subblk_h = 1 << txh;

                for unit_y in 0..=unit_y_log {
                    for unit_x in 0..=unit_x_log {
                        let x_start = unit_x * cnt_x;
                        let x_end = (x_start + cnt_x).min(max_cnt_x);
                        let y_start = unit_y * cnt_y;
                        let y_end = (y_start + cnt_y).min(max_cnt_y);
                        for y in y_start..y_end {
                            for x in x_start..x_end {
                                let col_off = x << txw;
                                let row_off = y << txh;
                                let have_top = y != 0 || xd.up_available != 0;
                                let have_left = x != 0 || xd.left_available != 0;

                                // Units above the block that are decoded, with those to the right of it.
                                let mut above_available = c_int::from(have_top);
                                let mut have_top_right = 0;
                                if need_above {
                                    let xr = (xd.mb_to_right_edge >> 3) + ((4 << bw_log) - (col_off << 2) - (4 << txw));
                                    if need_right {
                                        let right_available = mi_col + (col_off + subblk_w) < xd.tile.mi_col_end;
                                        have_top_right = has_top_right(
                                            cm,
                                            bsize,
                                            mi_row,
                                            mi_col,
                                            c_int::from(have_top),
                                            c_int::from(right_available),
                                            mi.partition,
                                            tx_sizes[0],
                                            row_off,
                                            col_off,
                                            0,
                                            0,
                                        );
                                    }
                                    above_available = (if have_top { subblk_w.min(subblk_w + (xr >> 2)) } else { 0 })
                                        + (if have_top_right != 0 { subblk_w.min(xr >> 2) } else { 0 });
                                }

                                // The same to the left, with the units below.
                                let mut left_available = c_int::from(have_left);
                                let mut have_bottom_left = 0;
                                if need_left {
                                    let yd =
                                        (xd.mb_to_bottom_edge >> 3) + ((4 << bh_log) - (row_off << 2) - (4 << txh));
                                    if need_bot {
                                        let bottom_available =
                                            yd > 0 && (mi_row + row_off + subblk_h) < xd.tile.mi_row_end;
                                        have_bottom_left = has_bottom_left(
                                            cm,
                                            bsize,
                                            mi_row,
                                            mi_col,
                                            c_int::from(bottom_available),
                                            c_int::from(have_left),
                                            mi.partition,
                                            tx_sizes[0],
                                            row_off,
                                            col_off,
                                            0,
                                            0,
                                        );
                                    }
                                    left_available = (if have_left { subblk_h.min(subblk_h + (yd >> 2)) } else { 0 })
                                        + (if have_bottom_left != 0 { subblk_h.min(yd >> 2) } else { 0 });
                                }

                                let iter_grid_blk =
                                    plane.iter_grid.offset((subblk_w * x + subblk_h * y * iter_grid_stride) as isize);
                                let above_left = c_int::from(have_top && have_left && need_aboveleft);
                                let iter;
                                if td.intra_v2 != 0 {
                                    // -intrav2: no wavefront iteration. The block gets a step and a pass from the
                                    // same neighbours and counts as iteration 0; gen_pred_blocks.hlsl reads that
                                    // from this entry.
                                    intra_v2::place(
                                        &td.intra_v2_sched,
                                        (tile.gen_block_map_offset + IntraBlockOffset - IntraSizes) as u32,
                                        false,
                                        mi_col + col_off,
                                        mi_row + row_off,
                                        txw,
                                        txh,
                                        above_left,
                                        above_available,
                                        left_available,
                                        false,
                                        (type_idx_base - (IntraBlockOffset - IntraSizes)) as u32,
                                        *map.offset(type_idx_base as isize) as u32,
                                    );
                                    iter = 0;
                                    tile.gen_intra_iter_y = iter.max(tile.gen_intra_iter_y);
                                    *iter_grid_blk.offset((iter_grid_stride + subblk_w) as isize) = iter;
                                } else if let Some(edges) = wave.as_mut() {
                                    iter = wavefront_iteration_edges(
                                        &plane,
                                        &mut edges.planes[0],
                                        mi_col + col_off,
                                        mi_row + row_off,
                                        above_left,
                                        above_available,
                                        left_available,
                                        NO_ITERATION,
                                        tile.gen_intra_iter_y,
                                        tile.gen_intra_max_iter,
                                    );
                                    tile.gen_intra_iter_y = iter.max(tile.gen_intra_iter_y);
                                    luma_under = luma_under.max(iter);
                                    // The one entry of the grid that gen_pred_blocks.hlsl reads.
                                    *iter_grid_blk.offset((iter_grid_stride + subblk_w) as isize) = iter;
                                } else {
                                    iter = wavefront_iteration(
                                        &plane,
                                        iter_grid_blk,
                                        above_left,
                                        above_available,
                                        left_available,
                                        -1,
                                        tile.gen_intra_iter_y,
                                        tile.gen_intra_max_iter,
                                    );
                                    tile.gen_intra_iter_y = iter.max(tile.gen_intra_iter_y);
                                }

                                // The first block of an iteration the tile has not had: its lists start empty.
                                if iter > tile.gen_intra_iter_set {
                                    clear_iteration_lists(map, tile, iter);
                                }

                                let type_index = iter * IntraTypeCount + type_idx_base;
                                let list = map.offset(type_index as isize);
                                *index_arr.offset(tile.gen_index_ptr as isize) =
                                    ((take(list, 1) << 2) | have_top_right | (have_bottom_left << 1)) as u32;
                                tile.gen_index_ptr += 1;
                            }
                        }
                    }
                }
            } else if let Some(edges) = wave.as_mut() {
                // The luma of a palette block is not of the wavefront.
                edges.planes[0].pass(mi_col, mi_row, bw, bh);
            }

            if !uv_use_palette && !is_chroma_ref {
                let mi_col_uv = mi_col >> 1;
                let mi_row_uv = mi_row >> 1;
                let iter_grid_stride_uv = td.iter_grid_stride_uv;
                let txw =
                    if is_intrabc || is_inter_intra { bw_log_uv } else { TX_SIZE_WIDE_LOG2[tx_sizes[1] as usize] - 2 };
                let txh =
                    if is_intrabc || is_inter_intra { bh_log_uv } else { TX_SIZE_HIGH_LOG2[tx_sizes[1] as usize] - 2 };
                let plane = IntraPlane {
                    iter_grid: td.gen_intra_iter_uv.offset((mi_col_uv + mi_row_uv * iter_grid_stride_uv) as isize),
                    iter_grid_stride: iter_grid_stride_uv,
                    mode: if is_inter_intra { interintra_mode } else { u32::from(mi.uv_mode) },
                    txw,
                    txh,
                    is_intrabc,
                };

                let need_above = plane.need(NEED_ABOVE_LUT) && !is_intrabc;
                let need_left = plane.need(NEED_LEFT_LUT) && !is_intrabc;
                let need_right = plane.need(NEED_RIGHT_LUT);
                let need_bot = plane.need(NEED_BOT_LUT);
                let need_aboveleft = plane.need(NEED_ABOVE_LEFT_LUT) && !is_intrabc;
                let is_cfl = plane.mode == UV_CFL_PRED as u32;

                let cnt_y = 1 << (bh_log_uv - txh - unit_y_log);
                let cnt_x = 1 << (bw_log_uv - txw - unit_x_log);

                let type_size = txw + txh;
                let subblk_w = 1 << txw;
                let subblk_h = 1 << txh;

                for unit_y in 0..=unit_y_log {
                    for unit_x in 0..=unit_x_log {
                        for suby in 0..cnt_y {
                            for subx in 0..cnt_x {
                                let x = subx + unit_x * cnt_x;
                                let y = suby + unit_y * cnt_y;
                                let col_off = x << txw;
                                let row_off = y << txh;
                                let have_top = y != 0 || xd.chroma_up_available != 0;
                                let have_left = x != 0 || xd.chroma_left_available != 0;
                                let mut above_available = c_int::from(have_top);
                                let mut have_top_right = 0;
                                if need_above {
                                    let xr =
                                        (xd.mb_to_right_edge >> 4) + ((4 << bw_log_uv) - (col_off << 2) - (4 << txw));
                                    if need_right {
                                        let right_available = mi_col + ((col_off + subblk_w) << 1) < xd.tile.mi_col_end;
                                        have_top_right = has_top_right(
                                            cm,
                                            bsize_uv,
                                            mi_row,
                                            mi_col,
                                            c_int::from(have_top),
                                            c_int::from(right_available),
                                            mi.partition,
                                            tx_sizes[1],
                                            row_off,
                                            col_off,
                                            1,
                                            1,
                                        );
                                    }
                                    above_available = (if have_top { subblk_w.min(subblk_w + (xr >> 2)) } else { 0 })
                                        + (if have_top_right != 0 { subblk_w.min(xr >> 2) } else { 0 });
                                }

                                let mut left_available = c_int::from(have_left);
                                let mut have_bottom_left = 0;
                                if need_left {
                                    let yd =
                                        (xd.mb_to_bottom_edge >> 4) + ((4 << bh_log_uv) - (row_off << 2) - (4 << txh));
                                    if need_bot {
                                        let bottom_available =
                                            yd > 0 && (mi_row + ((row_off + subblk_h) << 1)) < xd.tile.mi_row_end;
                                        have_bottom_left = has_bottom_left(
                                            cm,
                                            bsize_uv,
                                            mi_row,
                                            mi_col,
                                            c_int::from(bottom_available),
                                            c_int::from(have_left),
                                            mi.partition,
                                            tx_sizes[1],
                                            row_off,
                                            col_off,
                                            1,
                                            1,
                                        );
                                    }
                                    left_available = (if have_left { subblk_h.min(subblk_h + (yd >> 2)) } else { 0 })
                                        + (if have_bottom_left != 0 { subblk_h.min(yd >> 2) } else { 0 });
                                }

                                let iter_grid_blk = plane
                                    .iter_grid
                                    .offset((subblk_w * x + subblk_h * y * iter_grid_stride_uv) as isize);
                                let above_left = c_int::from(need_aboveleft && have_top && have_left);
                                let iter;
                                if td.intra_v2 != 0 {
                                    // -intrav2, as for luma. U and V share the place; CFL also waits for the luma
                                    // under the block.
                                    intra_v2::place(
                                        &td.intra_v2_sched,
                                        (tile.gen_block_map_offset + IntraBlockOffset - IntraSizes) as u32,
                                        true,
                                        mi_col_uv + col_off,
                                        mi_row_uv + row_off,
                                        txw,
                                        txh,
                                        above_left,
                                        above_available,
                                        left_available,
                                        is_cfl,
                                        (IntraSizes - 1 - type_size) as u32,
                                        *map.offset((IntraBlockOffset - 1 - type_size) as isize) as u32,
                                    );
                                    iter = 0;
                                    tile.gen_intra_iter_uv = iter.max(tile.gen_intra_iter_uv);
                                    *iter_grid_blk.offset((iter_grid_stride_uv + subblk_w) as isize) = iter;
                                } else if let Some(edges) = wave.as_mut() {
                                    iter = wavefront_iteration_edges(
                                        &plane,
                                        &mut edges.planes[1],
                                        mi_col_uv + col_off,
                                        mi_row_uv + row_off,
                                        above_left,
                                        above_available,
                                        left_available,
                                        // A CFL block also reads the luma under it.
                                        if is_cfl { luma_under } else { NO_ITERATION },
                                        tile.gen_intra_iter_uv,
                                        tile.gen_intra_max_iter,
                                    );
                                    tile.gen_intra_iter_uv = iter.max(tile.gen_intra_iter_uv);
                                    // The one entry of the grid that gen_pred_blocks.hlsl reads.
                                    *iter_grid_blk.offset((iter_grid_stride_uv + subblk_w) as isize) = iter;
                                } else {
                                    // A CFL block also reads the luma under it.
                                    let mut luma_iter = -1;
                                    if is_cfl {
                                        let iter_grid_stride = td.iter_grid_stride;
                                        let y_grid = td.gen_intra_iter_y.offset(
                                            (((mi_col_uv + col_off) << 1)
                                                + ((mi_row_uv + row_off) << 1) * iter_grid_stride)
                                                as isize,
                                        );
                                        for yit in 1..=(subblk_h << 1) {
                                            for xit in 1..=(subblk_w << 1) {
                                                luma_iter = luma_iter
                                                    .max(*y_grid.offset((xit + yit * iter_grid_stride) as isize));
                                            }
                                        }
                                    }
                                    iter = wavefront_iteration(
                                        &plane,
                                        iter_grid_blk,
                                        above_left,
                                        above_available,
                                        left_available,
                                        luma_iter,
                                        tile.gen_intra_iter_uv,
                                        tile.gen_intra_max_iter,
                                    );
                                    tile.gen_intra_iter_uv = iter.max(tile.gen_intra_iter_uv);
                                }

                                // The first block of an iteration the tile has not had: its lists start empty.
                                if iter > tile.gen_intra_iter_set {
                                    clear_iteration_lists(map, tile, iter);
                                }

                                let type_index = iter * IntraTypeCount + IntraBlockOffset - 1 - type_size;
                                let list = map.offset(type_index as isize);
                                *index_arr.offset(tile.gen_index_ptr as isize) =
                                    ((take(list, 2) << 2) | have_top_right | (have_bottom_left << 1)) as u32;
                                tile.gen_index_ptr += 1;
                            }
                        }
                    }
                }
            } else if !is_chroma_ref && let Some(edges) = wave.as_mut() {
                // The chroma of a palette block.
                edges.planes[1].pass(mi_col >> 1, mi_row >> 1, 1 << bw_log_uv, 1 << bh_log_uv);
            }
        } else if let Some(edges) = wave.as_mut()
            && edges.idle == 0
        {
            // An inter block is predicted before the wavefront: the intra blocks next to it do not wait
            // for it. (Nothing to enter while the tile is idle.)
            edges.planes[0].pass(mi_col, mi_row, bw, bh);
            if !is_chroma_ref {
                edges.planes[1].pass(mi_col >> 1, mi_row >> 1, 1 << bw_log_uv, 1 << bh_log_uv);
            }
        }

        // A block with residuals whose prediction is not written together with them (palette, overlapped
        // prediction) gets a reconstruction block.
        if mi.skip == 0 {
            if y_use_palette || is_obmc {
                let type_index = ReconBlockOffset + bw_log + 6 * bh_log;
                *index_arr.offset(tile.gen_index_ptr as isize) = take(map.offset(type_index as isize), 1) as u32;
                tile.gen_index_ptr += 1;
            }
            if uv_use_palette || (is_obmc && !is_chroma_ref) {
                let type_index = ReconBlockOffset + bw_log_uv + 6 * bh_log_uv;
                *index_arr.offset(tile.gen_index_ptr as isize) = take(map.offset(type_index as isize), 2) as u32;
                tile.gen_index_ptr += 1;
            }
        }
    }
}

/// The wavefront iterations whose lists are cleared at once when a tile reaches an iteration whose lists
/// are not empty yet: 40 KiB of counters.
const ITERATION_CLEAR_STEP: c_int = 1024;

/// The lists of the wavefront iterations a tile has not had start empty. The setup of a tile clears those
/// of its first iterations (`gen_intra_iter_set`, and 16 more: `setup_tile`); a block of a later
/// iteration clears the lists from where the cleared ones end up to those of
/// [`ITERATION_CLEAR_STEP`] iterations past its own, or to the end of the tile's lists. Everything that
/// reads the lists reads those of the iterations the tile has reached, and one counter more (the
/// recording, `rav1g_decode_frame_record`).
///
/// The C++ cleared all the lists the tile can have on the first such block: 5.2 MB for a 4K frame with
/// one tile, in every frame whose intra blocks reach 257 iterations (a third of the frames of a heavy 4K
/// clip), where the frame then uses a few thousand counters.
///
/// # Safety
/// `map` is the block map of `tile`, whose lists are as its setup left them or as this function did.
#[cold]
#[inline(never)]
unsafe fn clear_iteration_lists(map: *mut c_int, tile: &mut av1_tile_data, iter: c_int) {
    let max_iter = tile.gen_intra_max_iter;
    // Where the lists of the tile end, as the setup has it: `gen_iter_clear_offset` and the bytes behind.
    let end_of_lists = tile.gen_iter_clear_offset + tile.gen_iter_clear_size / size_of::<c_int>() as c_int;
    let reached = iter.saturating_add(ITERATION_CLEAR_STEP).min(max_iter);
    // The lists of the iterations up to `reached`, and of 16 more, as the setup clears them.
    let end = if reached >= max_iter { end_of_lists } else { ((reached + 16) * 10 + 256).min(end_of_lists) };
    let from = tile.gen_iter_clear_offset;
    if end > from {
        // SAFETY: the caller's contract: inside the tile's part of the block map.
        unsafe { map.offset(from as isize).write_bytes(0, (end - from) as usize) };
        tile.gen_iter_clear_offset = end;
        tile.gen_iter_clear_size = (end_of_lists - end) * size_of::<c_int>() as c_int;
    }
    tile.gen_intra_iter_set = reached;
}

/// The wavefront iteration of a transform block: one more than the highest iteration of the units it
/// reads (the row above from `1 - above_left` to `above_available`, the column to the left down to
/// `left_available`, and `other`, the luma under a CFL block), written to the units along its right and
/// bottom edge, where the blocks that read it look.
///
/// An intra block copy block reads samples anywhere in what the tile has decoded, so it takes an
/// iteration of its own behind the last one of the tile (`last_iter`). `max_iter` is where the tile's
/// lists end.
///
/// # Safety
/// `iter_grid_blk` is the grid of the plane at the block; the block and the units it reads are in it.
#[expect(clippy::too_many_arguments, reason = "what the two planes share of the C++ loop body")]
#[inline(always)]
unsafe fn wavefront_iteration(
    plane: &IntraPlane,
    iter_grid_blk: *mut c_int,
    above_left: c_int,
    above_available: c_int,
    left_available: c_int,
    other: c_int,
    last_iter: c_int,
    max_iter: c_int,
) -> c_int {
    let stride = plane.iter_grid_stride as isize;
    let subblk_w = 1isize << plane.txw;
    let subblk_h = 1isize << plane.txh;
    // SAFETY: the caller's contract.
    unsafe {
        let mut iter = -1;
        for it in (1 - above_left) as isize..=above_available as isize {
            iter = iter.max(*iter_grid_blk.offset(it));
        }
        for it in 1..=left_available as isize {
            iter = iter.max(*iter_grid_blk.offset(it * stride));
        }
        iter = iter.max(other);
        iter += 1;
        if plane.is_intrabc {
            iter = last_iter + 1;
        }
        iter = iter.min(max_iter);
        for it in 1..=subblk_h {
            *iter_grid_blk.offset(it * stride + subblk_w) = iter;
        }
        for it in 1..=subblk_w {
            *iter_grid_blk.offset(it + subblk_h * stride) = iter;
        }
        iter
    }
}

/// [`wavefront_iteration`] with the iterations of the neighbours at the edges of the tile instead of in
/// the wavefront grid: the transform block of `plane` at `col`, `row` of the plane takes its iteration
/// from the units the grid version reads (the row above from the unit above and to the left, if
/// `above_left`, to `above_available` units; `left_available` units of the column to the left; `other`),
/// and is entered at the edges with it.
///
/// # Safety
/// `edges` are the edges of the block's tile in the plane, with every block the tile has decoded; the
/// units over the block's width in the row above are decoded if `above_available` is not 0, the ones over
/// its height in the column to its left if `left_available` is not 0, and the corner if `above_left`.
#[expect(clippy::too_many_arguments, reason = "the arguments of wavefront_iteration, and where the block is")]
#[inline(always)]
unsafe fn wavefront_iteration_edges(
    plane: &IntraPlane,
    edges: &mut WavePlane,
    col: c_int,
    row: c_int,
    above_left: c_int,
    above_available: c_int,
    left_available: c_int,
    other: c_int,
    last_iter: c_int,
    max_iter: c_int,
) -> c_int {
    let (w, h) = (1 << plane.txw, 1 << plane.txh);
    // SAFETY: the caller's contract: the columns read are columns of the tile.
    unsafe {
        let above = edges.above.offset(col as isize);
        let mut iter = other;
        // (The grid version reads the row above from the corner to the last unit that counts: for a
        // chroma block outside the frame, whose count is below 0, not even the corner.)
        if above_left != 0 && above_available >= 0 {
            iter = iter.max(edges.rows[row as usize & WAVE_ROW_MASK][CORNER]);
        }
        // Over the width of the block the row above is decoded; further to the right a unit counts if it
        // is the decoded one of its column.
        for it in 0..above_available.min(w) {
            iter = iter.max(*above.offset(it as isize));
        }
        if above_available > w {
            let above_end = edges.above_end.offset(col as isize);
            let here = edges.stamp | i64::from(row);
            for it in w..above_available {
                if *above_end.offset(it as isize) == here {
                    iter = iter.max(*above.offset(it as isize));
                }
            }
        }
        // The same for the column to the left, over the height of the block and further down. (The
        // counts are below 0 for a chroma block outside the frame: nothing is read then.)
        for it in 0..left_available.min(h) {
            iter = iter.max(edges.rows[(row + it) as usize & WAVE_ROW_MASK][LEFT]);
        }
        if left_available > h {
            let here = edges.stamp | i64::from(col);
            for it in h..left_available {
                if edges.left_end[(row + it) as usize & WAVE_ROW_MASK] == here {
                    iter = iter.max(edges.rows[(row + it) as usize & WAVE_ROW_MASK][LEFT]);
                }
            }
        }
        iter = iter.max(NO_ITERATION) + 1;
        if plane.is_intrabc {
            iter = last_iter + 1;
        }
        iter = iter.min(max_iter);
        edges.commit(col, row, w, h, iter);
        iter
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sys::probe::{
        rav1g_probe_extract_interp_filter, rav1g_probe_is_interintra_pred, rav1g_probe_max_uv_txsize,
        rav1g_probe_scale_chroma_bsize,
    };

    #[test]
    fn inline_functions_of_the_c_headers() {
        // SAFETY: plain functions of numbers.
        unsafe {
            for bsize in 0..22 {
                assert_eq!(
                    c_int::from(scale_chroma_bsize_420(bsize as u8)),
                    rav1g_probe_scale_chroma_bsize(bsize, 1, 1)
                );
                for (ss_x, ss_y) in [(0, 0), (1, 1)] {
                    assert_eq!(
                        c_int::from(max_uv_txsize(bsize as u8, ss_x, ss_y)),
                        rav1g_probe_max_uv_txsize(bsize, ss_x, ss_y),
                        "block size {bsize}, subsampling {ss_x} {ss_y}"
                    );
                }
                for mode in 0..25 {
                    for ref0 in -1..8 {
                        for ref1 in -1..8 {
                            assert_eq!(
                                is_interintra_pred(bsize as u8, mode as u8, [ref0 as i8, ref1 as i8]),
                                rav1g_probe_is_interintra_pred(bsize, mode, ref0, ref1) != 0,
                                "block size {bsize}, mode {mode}, references {ref0} {ref1}"
                            );
                        }
                    }
                }
            }
            for filters in [0u32, 0x0001_0002, 0x0003_0000, 0x0002_0003, 0x000f_000f] {
                assert_eq!(
                    extract_interp_filter(filters, true) as c_int,
                    rav1g_probe_extract_interp_filter(filters, 1)
                );
                assert_eq!(
                    extract_interp_filter(filters, false) as c_int,
                    rav1g_probe_extract_interp_filter(filters, 0)
                );
            }
        }
    }
}
