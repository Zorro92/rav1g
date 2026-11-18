// SPDX-License-Identifier: BSD-2-Clause
//
// This source code is subject to the terms of the BSD 2 Clause License and the Alliance for Open Media
// Patent License 1.0. If the BSD 2 Clause License was not distributed with this source code in the LICENSE
// file, you can obtain it at www.aomedia.org/license/software. If the Alliance for Open Media Patent
// License 1.0 was not distributed with this source code in the PATENTS file, you can obtain it at
// www.aomedia.org/license/patent.

//! The mode info record of a block (`MB_MODE_INFO`, read by the shaders as `dx/shaders/mode_info.h`)
//! from a block of rav1d's parse pass.
//!
//! The entropy decoder of libaom filled the record while it read the block (`av1/decoder/decodemv.c`,
//! `decodeframe.c`). The same bitstream gives rav1d the same syntax elements, in its own form
//! (`Av1Block` of `rav1d::levels`); this file turns one into the other. Where libaom derives a field in a
//! way of its own, the function that does it here says so. The records were held against libaom's on
//! the conformance streams while its sources were in the tree; `tools/rav1g-parse-check` now holds
//! them equal for every way of running the parser.

use rav1d::include::dav1d::headers::{Rav1dFrameHeader, Rav1dTxfmMode, Rav1dWarpedMotionType};
use rav1d::levels::{Av1BlockInter, Av1BlockIntra, Av1BlockIntraInter, CompInterType, InterIntraType, MotionMode};
use rav1d::parse::ParseBlock;
use rav1g_sys::MB_MODE_INFO;

/// `BLOCK_SIZE` of libaom for a `BlockSize` of rav1d (which lists the sizes from the largest).
pub const BLOCK_SIZE_OF: [u8; 22] = [15, 14, 13, 12, 11, 21, 10, 9, 8, 19, 20, 7, 6, 5, 17, 18, 4, 3, 2, 16, 1, 0];

/// `mi_size_wide_log2` and `mi_size_high_log2` (`av1/common/common_data.h`) by `BLOCK_SIZE`.
pub const MI_SIZE_WIDE_LOG2: [u8; 22] = [0, 0, 1, 1, 1, 2, 2, 2, 3, 3, 3, 4, 4, 4, 5, 5, 0, 2, 1, 3, 2, 4];
pub const MI_SIZE_HIGH_LOG2: [u8; 22] = [0, 1, 0, 1, 2, 1, 2, 3, 2, 3, 4, 3, 4, 5, 4, 5, 2, 0, 3, 1, 4, 2];
/// `max_txsize_rect_lookup` by `BLOCK_SIZE`.
const MAX_TXSIZE_RECT_LOOKUP: [u8; 22] = [0, 5, 6, 1, 7, 8, 2, 9, 10, 3, 11, 12, 4, 4, 4, 4, 13, 14, 15, 16, 17, 18];
/// `sub_tx_size_map` by `TX_SIZE`.
const SUB_TX_SIZE_MAP: [u8; 19] = [0, 0, 1, 2, 3, 0, 0, 1, 1, 2, 2, 3, 3, 5, 6, 7, 8, 9, 10];
/// `tx_size_wide_unit` and `tx_size_high_unit` by `TX_SIZE`: the sides of a transform in 4x4 units.
const TX_SIZE_WIDE_UNIT: [i32; 19] = [1, 2, 4, 8, 16, 1, 2, 2, 4, 4, 8, 8, 16, 1, 4, 2, 8, 4, 16];
const TX_SIZE_HIGH_UNIT: [i32; 19] = [1, 2, 4, 8, 16, 2, 1, 4, 2, 8, 4, 16, 8, 4, 1, 8, 2, 16, 4];

const TX_4X4: u8 = 0;
const INTRA_FRAME: i8 = 0;
const NONE_FRAME: i8 = -1;
/// `PREDICTION_MODE`: the first single inter mode and the first compound one.
const NEARESTMV: u8 = 13;
const NEAREST_NEARESTMV: u8 = 17;
/// `PARTITION_NONE` and the partition of rav1d that splits a block in four.
const PARTITION_NONE: u8 = 0;
const PARTITION_SPLIT: u8 = 3;
/// `COMPOUND_TYPE`.
const COMPOUND_AVERAGE: u8 = 0;
const COMPOUND_DISTWTD: u8 = 1;
const COMPOUND_WEDGE: u8 = 2;
const COMPOUND_DIFFWTD: u8 = 3;
/// `BILINEAR` of `InterpFilter`, for both directions.
const INTERP_BILINEAR_BOTH: [u8; 2] = [3, 3];

/// What the record of a block needs of its frame.
pub struct RecordFrame<'a> {
    pub hdr: &'a Rav1dFrameHeader,
    /// Width and height of the frame in 4x4 units.
    pub mi_cols: i32,
    pub mi_rows: i32,
    /// The frame has chroma planes (not monochrome).
    pub has_chroma: bool,
}

/// `inter_tx_size` and `tx_size` of an inter block whose transform sizes were coded as a tree
/// (`read_tx_size_vartx` of `av1/decoder/decodeframe.c`), from rav1d's split bits.
///
/// libaom walks the tree of every largest transform of the block and writes the size of each leaf into
/// the entries of `inter_tx_size` it covers; `tx_size` is left as the size of the last leaf. A part of
/// the tree that lies outside the frame is not walked, so its entries keep what the record had (zero in
/// a cleared record): this is reproduced, not tidied.
///
/// The walk has the two fields by themselves, and the record gets them when it is through: the walk
/// calls itself, so it is not inlined, and with the record's address in it the compiler would keep less
/// of the fill in registers (the sink's calls were 2 to 7 % slower that way).
struct VarTx {
    /// `inter_tx_size` and `tx_size` of the record: what a cleared record has, and then what the walk
    /// writes.
    inter_tx_size: [u8; 16],
    tx_size: u8,
    split: [u16; 2],
    /// The smallest transform the tree of this block size can reach, in 4x4 units, as shifts.
    tx_w_log2: i32,
    tx_h_log2: i32,
    stride_log2: i32,
    min_txs: usize,
    /// The part of the block inside the frame, in 4x4 units.
    max_blocks_wide: i32,
    max_blocks_high: i32,
}

impl VarTx {
    /// `set_inter_tx_size`.
    fn set(&mut self, split_size: usize, txs: u8, blk_row: i32, blk_col: i32) {
        let mut idy = 0;
        while idy < TX_SIZE_HIGH_UNIT[split_size] {
            let mut idx = 0;
            while idx < TX_SIZE_WIDE_UNIT[split_size] {
                let index =
                    (((blk_row + idy) >> self.tx_h_log2) << self.stride_log2) + ((blk_col + idx) >> self.tx_w_log2);
                self.inter_tx_size[index as usize & 15] = txs;
                idx += TX_SIZE_WIDE_UNIT[self.min_txs];
            }
            idy += TX_SIZE_HIGH_UNIT[self.min_txs];
        }
    }

    /// `read_tx_size_vartx`; `x_off`, `y_off` are the position of the transform at this depth, which is
    /// where rav1d has its split bit (`read_tx_tree` of `rav1d/src/decode.rs`).
    fn walk(&mut self, tx_size: u8, depth: usize, blk_row: i32, blk_col: i32, x_off: i32, y_off: i32) {
        if blk_row >= self.max_blocks_high || blk_col >= self.max_blocks_wide {
            return;
        }
        let tx = tx_size as usize;
        let is_split = depth < 2 && self.split[depth] & (1 << (y_off * 4 + x_off)) != 0;
        if !is_split {
            self.set(tx, tx_size, blk_row, blk_col);
            self.tx_size = tx_size;
            return;
        }
        let sub = SUB_TX_SIZE_MAP[tx];
        if sub == TX_4X4 {
            self.set(tx, sub, blk_row, blk_col);
            self.tx_size = sub;
            return;
        }
        let bsw = TX_SIZE_WIDE_UNIT[sub as usize];
        let bsh = TX_SIZE_HIGH_UNIT[sub as usize];
        let mut row = 0;
        while row < TX_SIZE_HIGH_UNIT[tx] {
            let mut col = 0;
            while col < TX_SIZE_WIDE_UNIT[tx] {
                self.walk(sub, depth + 1, blk_row + row, blk_col + col, x_off * 2 + col / bsw, y_off * 2 + row / bsh);
                col += bsw;
            }
            row += bsh;
        }
    }
}

/// The transform size fields of an inter block (intra block copy included).
#[inline(always)]
fn inter_tx(
    mi: &mut MB_MODE_INFO,
    frame: &RecordFrame<'_>,
    bsize: u8,
    skip: bool,
    lossless: bool,
    inter: &Av1BlockInter,
) {
    let b = bsize as usize;
    let max_tx = MAX_TXSIZE_RECT_LOOKUP[b];
    if frame.hdr.txfm_mode == Rav1dTxfmMode::Switchable && bsize != 0 && !skip && !lossless {
        // The sizes were coded as a tree, one for every largest transform of the block.
        // The entries of `inter_tx_size` are the transforms one level under the largest
        // (`MAX_VARTX_DEPTH - 1` levels, `av1_get_txb_size_index`).
        let index_txs = SUB_TX_SIZE_MAP[max_tx as usize] as usize;
        let tx_w_log2 = TX_SIZE_WIDE_UNIT[index_txs].trailing_zeros() as i32;
        let tx_h_log2 = TX_SIZE_HIGH_UNIT[index_txs].trailing_zeros() as i32;
        let bw_log2 = i32::from(MI_SIZE_WIDE_LOG2[b]);
        let width = 1 << bw_log2;
        let height = 1 << i32::from(MI_SIZE_HIGH_LOG2[b]);
        let mi_row = i32::from(mi.mi_row);
        let mi_col = i32::from(mi.mi_col);
        let mut walker = VarTx {
            inter_tx_size: [0; 16],
            tx_size: 0,
            split: [u16::from(inter.tx_split0), inter.tx_split1],
            tx_w_log2,
            tx_h_log2,
            stride_log2: bw_log2 - tx_w_log2,
            min_txs: index_txs,
            max_blocks_wide: width.min(frame.mi_cols - mi_col),
            max_blocks_high: height.min(frame.mi_rows - mi_row),
        };
        let bw = TX_SIZE_WIDE_UNIT[max_tx as usize];
        let bh = TX_SIZE_HIGH_UNIT[max_tx as usize];
        let mut idy = 0;
        while idy < height {
            let mut idx = 0;
            while idx < width {
                walker.walk(max_tx, 0, idy, idx, idx / bw, idy / bh);
                idx += bw;
            }
            idy += bh;
        }
        mi.inter_tx_size = walker.inter_tx_size;
        mi.tx_size = walker.tx_size;
    } else {
        // `read_tx_size`: 4x4 in a lossless segment, else the largest of the block (a skipped inter block
        // has no size coded even where the frame codes sizes; a frame of 4x4 transforms has 4x4).
        let tx_size = if lossless || frame.hdr.txfm_mode == Rav1dTxfmMode::Only4x4 { TX_4X4 } else { max_tx };
        mi.tx_size = tx_size;
        mi.inter_tx_size = [tx_size; 16];
    }
}

/// The fields of an intra block that are its own.
#[inline(always)]
fn intra(mi: &mut MB_MODE_INFO, intra: &Av1BlockIntra, lossless: bool) {
    const FILTER_PRED: u8 = 13;
    const CFL_PRED: u8 = 13;
    mi.ref_frame = [INTRA_FRAME, NONE_FRAME];
    if intra.y_mode == FILTER_PRED {
        // A filter intra block is a DC block with the filter named next to it.
        mi.mode = 0;
        mi.flags = MB_MODE_INFO::USE_FILTER_INTRA;
        mi.filter_intra_mode = intra.y_angle as _;
    } else {
        mi.mode = intra.y_mode;
        mi.angle_delta[0] = intra.y_angle;
    }
    mi.uv_mode = intra.uv_mode;
    if intra.uv_mode == CFL_PRED {
        // The index and the joint sign of the two alphas, back from their values (`read_cfl_alphas`).
        let sign = |alpha: i8| match alpha {
            0 => 0,
            a if a < 0 => 1,
            _ => 2,
        };
        let magnitude = |alpha: i8| if alpha == 0 { 0 } else { alpha.unsigned_abs() - 1 };
        let [u, v] = intra.cfl_alpha;
        mi.cfl_alpha_signs = sign(u) * 3 + sign(v) - 1;
        mi.cfl_alpha_idx = (magnitude(u) << 4) | magnitude(v);
    } else {
        mi.angle_delta[1] = intra.uv_angle;
    }
    mi.palette_size = intra.pal_sz;
    mi.tx_size = if lossless { TX_4X4 } else { intra.tx as u8 };
}

/// The fields of an inter block that are its own.
#[inline(always)]
fn inter(mi: &mut MB_MODE_INFO, block: &ParseBlock<'_>, inter: &Av1BlockInter) {
    let is_comp = inter.comp_type.is_some();
    let interintra = inter.interintra_type.is_some();
    mi.ref_frame[0] = inter.r#ref[0] + 1;
    mi.ref_frame[1] = if is_comp {
        inter.r#ref[1] + 1
    } else if interintra {
        INTRA_FRAME
    } else {
        NONE_FRAME
    };
    mi.mode = inter.inter_mode + if is_comp { NEAREST_NEARESTMV } else { NEARESTMV };
    mi.motion_mode = inter.motion_mode as _;
    // The vectors. A block with a local warp keeps its vector in the other form of rav1d's union.
    let one_d = &inter.nd.one_d;
    if inter.motion_mode == MotionMode::Warp && !is_comp {
        let two_d = inter.nd.two_d();
        mi.mv[0].as_mv.row = two_d.mv2d.y;
        mi.mv[0].as_mv.col = two_d.mv2d.x;
    } else {
        mi.mv[0].as_mv.row = one_d.mv[0].y;
        mi.mv[0].as_mv.col = one_d.mv[0].x;
        if is_comp {
            mi.mv[1].as_mv.row = one_d.mv[1].y;
            mi.mv[1].as_mv.col = one_d.mv[1].x;
        }
    }
    let (h, v) = inter.filter2d.hv();
    mi.interp_filters = [v as u8, h as u8];

    let (compound_type, wedge_index, mut flags) = match inter.comp_type {
        None | Some(CompInterType::Avg) => (COMPOUND_AVERAGE, 0, 0),
        Some(CompInterType::WeightedAvg) => (COMPOUND_DISTWTD, 0, 0),
        Some(CompInterType::Wedge) => (COMPOUND_WEDGE, one_d.wedge_idx, one_d.mask_sign * MB_MODE_INFO::WEDGE_SIGN),
        Some(CompInterType::Seg) => (COMPOUND_DIFFWTD, 0, one_d.mask_sign * MB_MODE_INFO::MASK_TYPE),
    };
    mi.compound_type = compound_type;
    mi.wedge_index = wedge_index;
    if let Some(interintra_type) = inter.interintra_type {
        mi.interintra_mode = one_d.interintra_mode.get() as _;
        if interintra_type == InterIntraType::Wedge {
            mi.use_wedge_interintra = 1;
            mi.interintra_wedge_index = one_d.wedge_idx;
        }
    }
    if inter.motion_mode == MotionMode::Warp {
        // The local warp of the block (`find_projection` of libaom): the model the task derived from
        // the motion vectors around the block. Where no model can be fitted, or its shear is out of
        // range, libaom marks the model invalid and the block is predicted from its
        // motion vector; rav1d leaves the type of the task's model at identity for that. The numbers of
        // such a model mean nothing and are not written.
        let warp = block.warpmv;
        if warp.r#type == Rav1dWarpedMotionType::Affine {
            mi.wmmat = warp.matrix;
            mi.warp_angles = [warp.alpha(), warp.beta(), warp.gamma(), warp.delta()];
        } else {
            flags |= MB_MODE_INFO::WARP_INVALID;
        }
    }
    mi.flags = flags;
}

/// Fills the record of a block. `mi` is a cleared record; `lossless` says whether the block's segment
/// is lossless.
#[inline(always)]
pub fn fill(mi: &mut MB_MODE_INFO, frame: &RecordFrame<'_>, block: &ParseBlock<'_>, lossless: bool) {
    let b = block.b;
    let bsize = BLOCK_SIZE_OF[block.bs as usize];
    mi.sb_type = bsize;
    // The four 4x4 blocks of a split 8x8: libaom reads no partition under 8x8 and records none
    // (`decode_partition`); rav1d passes the split of the 8x8 on. A larger block is never the product of
    // a split itself (the split recurses), so this is the one case.
    mi.partition = if b.bp as u8 == PARTITION_SPLIT { PARTITION_NONE } else { b.bp as u8 };
    // AV1 frame dimensions are at most 65536 pixels, or 16384 units of 4x4.
    mi.mi_row = block.y4 as u16;
    mi.mi_col = block.x4 as u16;
    mi.segment_id = b.seg_id.get() as i8;
    mi.skip = b.skip as i8;
    if frame.hdr.delta.lf.present != 0 {
        if frame.hdr.delta.lf.multi != 0 {
            mi.delta_lf = block.delta_lf;
        } else {
            mi.delta_lf_from_base = block.delta_lf[0];
        }
    }
    match &b.ii {
        Av1BlockIntraInter::Intra(intra_block) => intra(mi, intra_block, lossless),
        Av1BlockIntraInter::Inter(inter_block) if block.intrabc => {
            // Intra block copy: an intra block of DC modes with a vector, read with bilinear filters.
            mi.ref_frame = [INTRA_FRAME, NONE_FRAME];
            mi.use_intrabc = 1;
            mi.interp_filters = INTERP_BILINEAR_BOTH;
            mi.mv[0].as_mv.row = inter_block.nd.one_d.mv[0].y;
            mi.mv[0].as_mv.col = inter_block.nd.one_d.mv[0].x;
            inter_tx(mi, frame, bsize, b.skip != 0, lossless, inter_block);
        }
        Av1BlockIntraInter::Inter(inter_block) => {
            inter(mi, block, inter_block);
            inter_tx(mi, frame, bsize, b.skip != 0, lossless, inter_block);
        }
    }
}
