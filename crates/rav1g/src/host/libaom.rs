// SPDX-License-Identifier: BSD-2-Clause
//
// Copyright (c) 2016, Alliance for Open Media. All rights reserved
//
// This source code is subject to the terms of the BSD 2 Clause License and the Alliance for Open Media
// Patent License 1.0. If the BSD 2 Clause License was not distributed with this source code in the LICENSE
// file, you can obtain it at www.aomedia.org/license/software. If the Alliance for Open Media Patent
// License 1.0 was not distributed with this source code in the PATENTS file, you can obtain it at
// www.aomedia.org/license/patent.

//! What the host needs of libaom's common code, in Rust: six functions that set a frame up for the
//! stages, and the constant tables the stages are given.
//!
//! While the entropy decoder of libaom was the decoder's parser, these were calls into its C
//! (`av1/common/av1_loopfilter.c`, `restoration.c`, `warped_motion.c`, `reconinter.c`) and symbols of
//! it. That C is not part of the decoder any more (docs/phase2-rav1d.md, step 6): linking the six
//! functions would bring the pixel code of libaom with them, which nothing here runs. The functions
//! below are those functions, statement for statement; the tables are read from the C sources by the
//! build script (`build.rs`), so they are the same numbers.

use std::ffi::c_int;
use std::sync::OnceLock;

use super::tables::{
    BLOCK_SIZE_HIGH, BLOCK_SIZE_WIDE, MI_SIZE_HIGH_LOG2, MI_SIZE_WIDE_LOG2, TX_SIZE_HIGH, TX_SIZE_WIDE,
};
use crate::sys::parser::{
    AV1_COMMON, AV1LrStruct, AV1PixelRect, RESTORE_NONE, RestorationInfo, WarpedMotionParams, YV12_BUFFER_CONFIG,
};

include!(concat!(env!("OUT_DIR"), "/libaom_tables.rs"));

/// `MAX_LOOP_FILTER` of `av1/common/av1_loopfilter.h`.
const MAX_LOOP_FILTER: c_int = 63;
/// `SEG_LVL_ALT_LF_Y_V`, `_Y_H`, `_U`, `_V` of `av1/common/seg_common.h`, by plane and direction
/// (`seg_lvl_lf_lut`).
const SEG_LVL_LF: [[usize; 2]; 3] = [[1, 2], [3, 3], [4, 4]];
/// `WARPEDMODEL_PREC_BITS`, `WARP_PARAM_REDUCE_BITS` of `av1/common/mv.h`; `DIV_LUT_BITS`,
/// `DIV_LUT_PREC_BITS` of `av1/common/warped_motion.c`.
const WARPEDMODEL_PREC_BITS: u32 = 16;
const WARP_PARAM_REDUCE_BITS: u32 = 6;
const DIV_LUT_BITS: i32 = 8;
const DIV_LUT_PREC_BITS: i32 = 14;

/// `update_sharpness` of `av1/common/av1_loopfilter.c`: the limits of every filter level for the
/// sharpness of the frame.
fn update_sharpness(cm: &mut AV1_COMMON) {
    let sharpness = cm.lf.sharpness_level;
    for level in 0..=MAX_LOOP_FILTER {
        let mut inside = level >> (c_int::from(sharpness > 0) + c_int::from(sharpness > 4));
        if sharpness > 0 && inside > 9 - sharpness {
            inside = 9 - sharpness;
        }
        let inside = inside.max(1);
        let thresholds = &mut cm.lf_info.lfthr[level as usize];
        thresholds.lim = [inside as u8; 16];
        thresholds.mblim = [(2 * (level + 2) + inside) as u8; 16];
    }
}

/// `av1_loop_filter_init`: the parts of the loop filter thresholds that do not change with the frame.
pub(crate) fn loop_filter_init(cm: &mut AV1_COMMON) {
    cm.lf.combine_vert_horz_lf = 1;
    update_sharpness(cm);
    for level in 0..=MAX_LOOP_FILTER {
        cm.lf_info.lfthr[level as usize].hev_thr = [(level >> 4) as u8; 16];
    }
}

/// `av1_loop_filter_frame_init` for the three planes: the filter level of every segment, direction,
/// reference and mode of the frame.
pub(crate) fn loop_filter_frame_init(cm: &mut AV1_COMMON) {
    update_sharpness(cm);
    let vertical = [cm.lf.filter_level[0], cm.lf.filter_level_u, cm.lf.filter_level_v];
    let horizontal = [cm.lf.filter_level[1], cm.lf.filter_level_u, cm.lf.filter_level_v];
    for plane in 0..3 {
        if plane == 0 && vertical[0] == 0 && horizontal[0] == 0 {
            break;
        }
        if plane > 0 && vertical[plane] == 0 {
            continue;
        }
        for segment in 0..8 {
            for direction in 0..2 {
                let mut level = if direction == 0 { vertical[plane] } else { horizontal[plane] };
                let feature = SEG_LVL_LF[plane][direction];
                if cm.seg.enabled != 0 && cm.seg.feature_mask[segment] & (1 << feature) != 0 {
                    level = (level + c_int::from(cm.seg.feature_data[segment][feature])).clamp(0, MAX_LOOP_FILTER);
                }
                let levels = &mut cm.lf_info.lvl[plane][segment][direction];
                if cm.lf.mode_ref_delta_enabled == 0 {
                    *levels = [[level as u8; 2]; 8];
                } else {
                    let scale = 1 << (level >> 5);
                    let intra = level + c_int::from(cm.lf.ref_deltas[0]) * scale;
                    levels[0][0] = intra.clamp(0, MAX_LOOP_FILTER) as u8;
                    for reference in 1..8 {
                        for mode in 0..2 {
                            let inter = level
                                + c_int::from(cm.lf.ref_deltas[reference]) * scale
                                + c_int::from(cm.lf.mode_deltas[mode]) * scale;
                            levels[reference][mode] = inter.clamp(0, MAX_LOOP_FILTER) as u8;
                        }
                    }
                }
            }
        }
    }
}

/// `av1_whole_frame_rect` of `av1/common/restoration.c`: the plane of the frame behind the upscale.
fn whole_frame_rect(cm: &AV1_COMMON, is_uv: bool) -> AV1PixelRect {
    let ss_x = c_int::from(is_uv && cm.seq_params.subsampling_x != 0);
    let ss_y = c_int::from(is_uv && cm.seq_params.subsampling_y != 0);
    // ROUND_POWER_OF_TWO.
    let round = |value: c_int, shift: c_int| (value + ((1 << shift) >> 1)) >> shift;
    AV1PixelRect { left: 0, top: 0, right: round(cm.superres_upscaled_width, ss_x), bottom: round(cm.height, ss_y) }
}

/// `av1_alloc_restoration_struct`: the loop restoration units of a plane, from the frame size and the
/// unit size. A unit at the right or bottom edge is up to half as large again, so the count is rounded
/// to the nearest, and at least one. (The function allocates nothing: the units are the host's.)
pub(crate) fn alloc_restoration_struct(cm: &AV1_COMMON, rsi: &mut RestorationInfo, is_uv: bool) {
    let rect = whole_frame_rect(cm, is_uv);
    let unit = rsi.restoration_unit_size;
    let count = |size: c_int| ((size + (unit >> 1)) / unit).max(1);
    let horizontal = count(rect.right - rect.left);
    let vertical = count(rect.bottom - rect.top);
    rsi.units_per_tile = horizontal * vertical;
    rsi.horz_units_per_tile = horizontal;
    rsi.vert_units_per_tile = vertical;
}

/// `av1_loop_restoration_filter_frame_init` for the three planes, without the unit function of the
/// software filter: what the planes with a restoration are filtered with.
///
/// # Safety
/// `frame` is the buffer description of the frame of `cm`; the three live as long as `lr_ctxt` is read.
pub(crate) unsafe fn loop_restoration_filter_frame_init(
    lr_ctxt: &mut AV1LrStruct,
    frame: *mut YV12_BUFFER_CONFIG,
    cm: &mut AV1_COMMON,
) {
    let bit_depth = cm.seq_params.bit_depth as c_int;
    let highbd = c_int::from(cm.seq_params.use_highbitdepth);
    lr_ctxt.dst = &raw mut cm.rst_frame;
    lr_ctxt.frame = frame;
    for plane in 0..3 {
        cm.rst_info[plane].optimized_lr = 0;
        if cm.rst_info[plane].frame_restoration_type as u32 == RESTORE_NONE as u32 {
            continue;
        }
        let is_uv = plane > 0;
        let rect = whole_frame_rect(cm, is_uv);
        let context = &mut lr_ctxt.ctxt[plane];
        context.rsi = &raw const cm.rst_info[plane];
        context.ss_x = c_int::from(is_uv && cm.seq_params.subsampling_x != 0);
        context.ss_y = c_int::from(is_uv && cm.seq_params.subsampling_y != 0);
        context.highbd = highbd;
        context.bit_depth = bit_depth;
        // SAFETY: the caller's contract; the planes of a buffer description are read as numbers.
        unsafe {
            context.data8 = (*frame).__bindgen_anon_6.buffers[plane];
            context.dst8 = cm.rst_frame.__bindgen_anon_6.buffers[plane];
            context.data_stride = (*frame).__bindgen_anon_5.strides[usize::from(is_uv)];
            context.dst_stride = cm.rst_frame.__bindgen_anon_5.strides[usize::from(is_uv)];
        }
        context.tile_rect = rect;
        context.tile_stripe0 = 0;
    }
}

/// `ROUND_POWER_OF_TWO_SIGNED_64` of `aom_ports/mem.h`.
fn round_signed_64(value: i64, shift: i32) -> i64 {
    let round = |value: i64| (value + ((1i64 << shift) >> 1)) >> shift;
    if value < 0 { -round(-value) } else { round(value) }
}

/// `get_shear_params` of `av1/common/warped_motion.c`: the four shears of a warp model, reduced to
/// multiples of 64. False for a model that is not valid: one whose matrix is not affine with a positive
/// diagonal, or whose shears are too large for the filter.
pub(crate) fn get_shear_params(wm: &mut WarpedMotionParams) -> bool {
    let mat = wm.wmmat;
    if mat[2] <= 0 {
        return false;
    }
    let clamp16 = |value: i64| value.clamp(i64::from(i16::MIN), i64::from(i16::MAX)) as i32;
    let alpha = clamp16(i64::from(mat[2]) - (1 << WARPEDMODEL_PREC_BITS));
    let beta = clamp16(i64::from(mat[3]));
    // resolve_divisor_32: the divisor from the table of reciprocals, and its shift.
    let divisor = mat[2].unsigned_abs();
    let msb = 31 - divisor.leading_zeros() as i32;
    let rest = (divisor - (1u32 << msb)) as i32;
    let index = if msb > DIV_LUT_BITS {
        (rest + ((1 << (msb - DIV_LUT_BITS)) >> 1)) >> (msb - DIV_LUT_BITS)
    } else {
        rest << (DIV_LUT_BITS - msb)
    };
    let shift = msb + DIV_LUT_PREC_BITS;
    let y = i64::from(DIV_LUT[index as usize]);
    let v = (i64::from(mat[4]) * (1 << WARPEDMODEL_PREC_BITS)) * y;
    // (int) of the rounded value, as the C has it.
    let gamma = clamp16(i64::from(round_signed_64(v, shift) as i32));
    let v = (i64::from(mat[3]) * i64::from(mat[4])) * y;
    let delta = clamp16(i64::from(
        mat[5].wrapping_sub(round_signed_64(v, shift) as i32).wrapping_sub(1 << WARPEDMODEL_PREC_BITS),
    ));
    // ROUND_POWER_OF_TWO_SIGNED, back to the precision, into the 16 bits of the field.
    let reduce = |value: i32| -> i16 {
        let half = (1 << WARP_PARAM_REDUCE_BITS) >> 1;
        let rounded = if value < 0 {
            -((-value + half) >> WARP_PARAM_REDUCE_BITS)
        } else {
            (value + half) >> WARP_PARAM_REDUCE_BITS
        };
        (rounded * (1 << WARP_PARAM_REDUCE_BITS)) as i16
    };
    wm.alpha = reduce(alpha);
    wm.beta = reduce(beta);
    wm.gamma = reduce(gamma);
    wm.delta = reduce(delta);
    // is_affine_shear_allowed.
    let abs = |value: i16| i32::from(value).abs();
    4 * abs(wm.alpha) + 7 * abs(wm.beta) < (1 << WARPEDMODEL_PREC_BITS)
        && 4 * abs(wm.gamma) + 4 * abs(wm.delta) < (1 << WARPEDMODEL_PREC_BITS)
}

/// `BLOCK_64X64`, `BLOCK_SIZES`, `PARTITION_VERT_A` and `_B` of `av1/common/enums.h`; `MAX_MIB_SIZE_LOG2`.
const BLOCK_64X64: usize = 12;
const BLOCK_SIZES: usize = 16;
const PARTITION_VERT_A: u8 = 6;
const PARTITION_VERT_B: u8 = 7;
const MAX_MIB_SIZE_LOG2: c_int = 5;

/// The bit of a block of a superblock in one of the edge tables, by the place of the block.
fn edge_bit(tables: &[&[u8]], vertical: &[&[u8]], partition: u8, bsize: usize, index: c_int) -> c_int {
    let table = if partition == PARTITION_VERT_A || partition == PARTITION_VERT_B {
        assert!(bsize < BLOCK_SIZES, "a vertical partition of a block size that has none");
        vertical[bsize]
    } else {
        tables[bsize]
    };
    c_int::from((table[(index / 8) as usize] >> (index % 8)) & 1)
}

/// `has_top_right` of `av1/common/reconintra.c`: the samples above and to the right of a transform
/// block are decoded before it. `row_off`, `col_off`: the transform block in its block, in 4x4 units of
/// the plane.
///
/// # Safety
/// `cm` is the header of the frame.
#[allow(clippy::too_many_arguments)]
pub(crate) unsafe fn has_top_right(
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
) -> c_int {
    if top_available == 0 || right_available == 0 {
        return 0;
    }
    let bsize = bsize as usize;
    let wide = c_int::from(BLOCK_SIZE_WIDE[bsize]);
    let plane_bw_unit = ((wide >> 2) >> ss_x).max(1);
    let top_right_count_unit = TX_SIZE_WIDE[txsz as usize] >> 2;
    // The 4x4 units of a 64x64 block.
    let units_64 = 16;
    if row_off > 0 {
        // Only whether there are enough samples to the right.
        if wide > c_int::from(BLOCK_SIZE_WIDE[BLOCK_64X64]) {
            if row_off == units_64 >> ss_y && col_off + top_right_count_unit == units_64 >> ss_x {
                return 1;
            }
            let plane_bw_unit_64 = units_64 >> ss_x;
            return c_int::from(col_off % plane_bw_unit_64 + top_right_count_unit < plane_bw_unit_64);
        }
        return c_int::from(col_off + top_right_count_unit < plane_bw_unit);
    }
    if col_off + top_right_count_unit < plane_bw_unit {
        return 1;
    }
    let bw_in_mi_log2 = c_int::from(MI_SIZE_WIDE_LOG2[bsize]);
    let bh_in_mi_log2 = c_int::from(MI_SIZE_HIGH_LOG2[bsize]);
    // SAFETY: the caller's contract.
    let sb_mi_size = c_int::from(BLOCK_SIZE_HIGH[unsafe { (*cm).seq_params.sb_size } as usize]) >> 2;
    let blk_row_in_sb = (mi_row & (sb_mi_size - 1)) >> bh_in_mi_log2;
    let blk_col_in_sb = (mi_col & (sb_mi_size - 1)) >> bw_in_mi_log2;
    // The top row of a superblock: its top right is the superblock above.
    if blk_row_in_sb == 0 {
        return 1;
    }
    // The rightmost column: its top right is the next superblock.
    if ((blk_col_in_sb + 1) << bw_in_mi_log2) >= sb_mi_size {
        return 0;
    }
    let index = (blk_row_in_sb << (MAX_MIB_SIZE_LOG2 - bw_in_mi_log2)) + blk_col_in_sb;
    edge_bit(&HAS_TR_TABLES, &HAS_TR_VERT_TABLES, partition, bsize, index)
}

/// `has_bottom_left` of `av1/common/reconintra.c`: the same for the samples to the left and below.
///
/// # Safety
/// `cm` is the header of the frame.
#[allow(clippy::too_many_arguments)]
pub(crate) unsafe fn has_bottom_left(
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
) -> c_int {
    if bottom_available == 0 || left_available == 0 {
        return 0;
    }
    let bsize = bsize as usize;
    let high_unit = TX_SIZE_HIGH[txsz as usize] >> 2;
    let units_64 = 16;
    // A block wider than 64: bottom left samples are there for the transform blocks at the left edge
    // of each of its 64x64 parts.
    if c_int::from(BLOCK_SIZE_WIDE[bsize]) > c_int::from(BLOCK_SIZE_WIDE[BLOCK_64X64]) && col_off > 0 {
        let plane_bw_unit_64 = units_64 >> ss_x;
        if col_off % plane_bw_unit_64 == 0 {
            let plane_bh_unit_64 = units_64 >> ss_y;
            let row_off_64 = row_off % plane_bh_unit_64;
            let plane_bh_unit = ((c_int::from(BLOCK_SIZE_HIGH[bsize]) >> 2) >> ss_y).min(plane_bh_unit_64);
            return c_int::from(row_off_64 + high_unit < plane_bh_unit);
        }
    }
    if col_off > 0 {
        return 0;
    }
    let plane_bh_unit = ((c_int::from(BLOCK_SIZE_HIGH[bsize]) >> 2) >> ss_y).max(1);
    if row_off + high_unit < plane_bh_unit {
        return 1;
    }
    let bw_in_mi_log2 = c_int::from(MI_SIZE_WIDE_LOG2[bsize]);
    let bh_in_mi_log2 = c_int::from(MI_SIZE_HIGH_LOG2[bsize]);
    // SAFETY: the caller's contract.
    let sb_mi_size = c_int::from(BLOCK_SIZE_HIGH[unsafe { (*cm).seq_params.sb_size } as usize]) >> 2;
    let blk_row_in_sb = (mi_row & (sb_mi_size - 1)) >> bh_in_mi_log2;
    let blk_col_in_sb = (mi_col & (sb_mi_size - 1)) >> bw_in_mi_log2;
    // The leftmost column of a superblock: its bottom left is the superblock to the left, which is
    // decoded down to its end.
    if blk_col_in_sb == 0 {
        let blk_start_row_off = (blk_row_in_sb << bh_in_mi_log2) >> ss_y;
        let row_off_in_sb = blk_start_row_off + row_off;
        return c_int::from(row_off_in_sb + high_unit < sb_mi_size >> ss_y);
    }
    // The bottom row: its bottom left is the next row of superblocks.
    if ((blk_row_in_sb + 1) << bh_in_mi_log2) >= sb_mi_size {
        return 0;
    }
    let index = (blk_row_in_sb << (MAX_MIB_SIZE_LOG2 - bw_in_mi_log2)) + blk_col_in_sb;
    edge_bit(&HAS_BL_TABLES, &HAS_BL_VERT_TABLES, partition, bsize, index)
}

/// `MASK_MASTER_SIZE` of `av1/common/reconinter.h`: the masters of the wedge masks are 64x64.
const MASTER: usize = 64;
/// The wedge directions (`WedgeDirectionType`).
const HORIZONTAL: usize = 0;
const VERTICAL: usize = 1;
const OBLIQUE27: usize = 2;
const OBLIQUE63: usize = 3;
const OBLIQUE117: usize = 4;
const OBLIQUE153: usize = 5;
/// The bytes of `wedge_mask_buf`: the masks of all block sizes, which the mask buffer of the decoder
/// is sized by (2 signs, 16 wedges, room for four 32x32 blocks).
pub(crate) const WEDGE_MASK_BUF_BYTES: usize = 2 * 16 * 4 * 32 * 32;

/// The masters of the wedge masks (`wedge_mask_obl` of `av1/common/reconinter.c`): for a mask and its
/// complement, the six directions, 64x64 weights of 0 to 64 each.
pub(crate) struct WedgeMasks {
    masters: Vec<u8>,
}

/// `shift_copy`: a row of a master, shifted, with its end value repeated.
fn shift_copy(source: &[u8; MASTER], row: &mut [u8], shift: i32) {
    if shift >= 0 {
        let shift = shift as usize;
        row[shift..].copy_from_slice(&source[..MASTER - shift]);
        row[..shift].fill(source[0]);
    } else {
        let shift = (-shift) as usize;
        row[..MASTER - shift].copy_from_slice(&source[shift..]);
        row[MASTER - shift..].fill(source[MASTER - 1]);
    }
}

impl WedgeMasks {
    fn at(complement: usize, direction: usize) -> usize {
        (complement * 6 + direction) * MASTER * MASTER
    }

    /// `init_wedge_master_masks`.
    fn new() -> WedgeMasks {
        let mut masters = vec![0u8; 2 * 6 * MASTER * MASTER];
        let mut shift = (MASTER / 4) as i32;
        for row in (0..MASTER).step_by(2) {
            let oblique = Self::at(0, OBLIQUE63);
            shift_copy(&WEDGE_MASTER_OBLIQUE_EVEN, &mut masters[oblique + row * MASTER..][..MASTER], shift);
            shift -= 1;
            shift_copy(&WEDGE_MASTER_OBLIQUE_ODD, &mut masters[oblique + (row + 1) * MASTER..][..MASTER], shift);
            let vertical = Self::at(0, VERTICAL);
            masters[vertical + row * MASTER..][..MASTER].copy_from_slice(&WEDGE_MASTER_VERTICAL);
            masters[vertical + (row + 1) * MASTER..][..MASTER].copy_from_slice(&WEDGE_MASTER_VERTICAL);
        }
        let full = 64u8;
        for i in 0..MASTER {
            for j in 0..MASTER {
                let mirrored = MASTER - 1 - j;
                let weight = masters[Self::at(0, OBLIQUE63) + i * MASTER + j];
                masters[Self::at(0, OBLIQUE27) + j * MASTER + i] = weight;
                masters[Self::at(0, OBLIQUE117) + i * MASTER + mirrored] = full - weight;
                masters[Self::at(0, OBLIQUE153) + mirrored * MASTER + i] = full - weight;
                masters[Self::at(1, OBLIQUE63) + i * MASTER + j] = full - weight;
                masters[Self::at(1, OBLIQUE27) + j * MASTER + i] = full - weight;
                masters[Self::at(1, OBLIQUE117) + i * MASTER + mirrored] = weight;
                masters[Self::at(1, OBLIQUE153) + mirrored * MASTER + i] = weight;
                let weight = masters[Self::at(0, VERTICAL) + i * MASTER + j];
                masters[Self::at(0, HORIZONTAL) + j * MASTER + i] = weight;
                masters[Self::at(1, VERTICAL) + i * MASTER + j] = full - weight;
                masters[Self::at(1, HORIZONTAL) + j * MASTER + i] = full - weight;
            }
        }
        WedgeMasks { masters }
    }

    /// The masters, built once.
    pub(crate) fn get() -> &'static WedgeMasks {
        static MASKS: OnceLock<WedgeMasks> = OnceLock::new();
        MASKS.get_or_init(WedgeMasks::new)
    }

    /// The wedge bits of a block size (`wedge_params_lookup[bsize].bits`): 0 for one without wedges.
    pub(crate) fn bits(bsize: usize) -> u8 {
        WEDGE_PARAMS[bsize][0]
    }

    /// The mask of the wedge `index` of a block size with the sign `sign`, `bw * bh` weights, row by
    /// row (`wedge_params_lookup[bsize].masks[sign][index]`, which `init_wedge_masks` copies out of the
    /// masters at `get_wedge_mask_inplace`).
    pub(crate) fn mask(&self, bsize: usize, sign: usize, index: usize) -> Vec<u8> {
        let bw = BLOCK_SIZE_WIDE[bsize] as usize;
        let bh = BLOCK_SIZE_HIGH[bsize] as usize;
        let code = WEDGE_CODEBOOKS[WEDGE_PARAMS[bsize][1] as usize][index];
        let flip = WEDGE_SIGNFLIP[bsize][index] as usize;
        let woff = (code[1] as usize * bw) >> 3;
        let hoff = (code[2] as usize * bh) >> 3;
        let start = Self::at(sign ^ flip, code[0] as usize) + MASTER * (MASTER / 2 - hoff) + MASTER / 2 - woff;
        let mut mask = Vec::with_capacity(bw * bh);
        for row in 0..bh {
            mask.extend_from_slice(&self.masters[start + row * MASTER..][..bw]);
        }
        mask
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_tables_are_those_of_the_sources() {
        // The first and the last entries of each, as the C sources have them.
        assert_eq!(WARPED_FILTER[0], [0, 0, 127, 1, 0, 0, 0, 0]);
        assert_eq!(WARPED_FILTER[1], [0, -1, 127, 2, 0, 0, 0, 0]);
        assert_eq!(WARPED_FILTER.len(), 193);
        assert_eq!((DIV_LUT[0], DIV_LUT[256]), (16384, 8192));
        assert_eq!((II_WEIGHTS1D[0], II_WEIGHTS1D[127]), (60, 1));
        assert_eq!(II_SIZE_SCALES, [32, 16, 16, 16, 8, 8, 8, 4, 4, 4, 2, 2, 2, 1, 1, 1, 8, 8, 4, 4, 2, 2]);
        assert_eq!(SGR_PARAMS[0], [2, 1, 140, 3236]);
        assert_eq!(SGR_PARAMS[10], [0, 1, -1, 2589]);
        assert_eq!(FILTER_INTRA_TAPS[0][0], [-6, 10, 0, 0, 0, 12, 0, 0]);
        assert_eq!(IDCT_SCANS[0][0], &[0, 1, 4, 8, 5, 2, 3, 6, 9, 12, 13, 10, 7, 11, 14, 15][..]);
        assert_eq!(IDCT_SCANS[0][1], &[0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15][..]);
        // 4x4, 8x8, 16x16, 32x32 and the 64x64 that is scanned as 32x32.
        assert_eq!([0, 1, 2, 3, 4].map(|size| IDCT_SCANS[size][0].len()), [16, 64, 256, 1024, 1024]);
        // The wedges: 8x8 (block size 3) has the codebook of equal sides; 4x4 has none.
        assert_eq!((WedgeMasks::bits(0), WedgeMasks::bits(3)), (0, 4));
        assert_eq!(WEDGE_PARAMS[3][1], 2);
        assert_eq!(WEDGE_CODEBOOKS[0][0], [OBLIQUE27 as u8, 4, 4]);
        assert_eq!(WEDGE_CODEBOOKS[2][7], [VERTICAL as u8, 6, 4]);
    }

    #[test]
    fn a_wedge_mask_and_its_complement_add_up() {
        let masks = WedgeMasks::get();
        for bsize in (0..22).filter(|&bsize| WedgeMasks::bits(bsize) != 0) {
            let size = BLOCK_SIZE_WIDE[bsize] as usize * BLOCK_SIZE_HIGH[bsize] as usize;
            for index in 0..16 {
                let (mask, complement) = (masks.mask(bsize, 0, index), masks.mask(bsize, 1, index));
                assert_eq!((mask.len(), complement.len()), (size, size));
                assert!(mask.iter().zip(&complement).all(|(a, b)| a + b == 64), "block size {bsize}, wedge {index}");
            }
        }
        // The vertical wedge in the middle of a 16x16 block (codebook of equal sides, entry 7 is at
        // 6/8; entry 6 at 2/8): every row is the same, and it rises from one side to the other.
        let mask = masks.mask(6, 0, 6);
        assert_eq!(&mask[..16], &mask[16..32]);
        assert!(mask[0] != mask[15]);
    }

    #[test]
    fn the_shears_of_a_warp_model() {
        // SAFETY: all zero is a model.
        let mut model: WarpedMotionParams = unsafe { std::mem::zeroed() };
        // The identity: no shear, valid.
        model.wmmat = [0, 0, 1 << 16, 0, 0, 1 << 16, 0, 0];
        assert!(get_shear_params(&mut model));
        assert_eq!((model.alpha, model.beta, model.gamma, model.delta), (0, 0, 0, 0));
        // A small rotation: alpha and delta from the diagonal, beta and gamma from the other two.
        model.wmmat = [0, 0, (1 << 16) + 640, 1280, -1280, (1 << 16) + 640, 0, 0];
        assert!(get_shear_params(&mut model));
        assert_eq!((model.alpha, model.beta), (640, 1280));
        // gamma = mat[4] / mat[2] in 16 bits of precision: -1280 * 65536 / 66176 = -1267.6, to 64s.
        assert_eq!(model.gamma, -1280);
        // delta = mat[5] - mat[3] * mat[4] / mat[2] - 65536 = 640 + 24.8, to 64s.
        assert_eq!(model.delta, 640);
        // Not affine (the diagonal is not positive), and shears the filter cannot do.
        model.wmmat = [0, 0, 0, 0, 0, 1 << 16, 0, 0];
        assert!(!get_shear_params(&mut model));
        model.wmmat = [0, 0, 1 << 16, 1 << 14, 0, 1 << 16, 0, 0];
        assert!(!get_shear_params(&mut model));
    }

    #[test]
    fn the_levels_of_the_loop_filter() {
        // SAFETY: all zero is a frame header.
        let mut cm: Box<AV1_COMMON> = unsafe { super::super::rav1d_frames::zeroed_box() };
        loop_filter_init(&mut cm);
        assert_eq!(cm.lf.combine_vert_horz_lf, 1);
        assert_eq!((cm.lf_info.lfthr[0].lim[0], cm.lf_info.lfthr[0].mblim[15]), (1, 5));
        assert_eq!(
            (cm.lf_info.lfthr[63].lim[0], cm.lf_info.lfthr[63].mblim[0], cm.lf_info.lfthr[63].hev_thr[0]),
            (63, 193, 3)
        );
        cm.lf.sharpness_level = 5;
        cm.lf.filter_level = [20, 40];
        cm.lf.filter_level_u = 10;
        cm.lf.mode_ref_delta_enabled = 1;
        cm.lf.ref_deltas = [1, 0, 0, 0, -1, 0, -1, -1];
        cm.lf.mode_deltas = [0, 2];
        loop_filter_frame_init(&mut cm);
        // Sharpness 5: the level shifted by two, at most 4.
        assert_eq!((cm.lf_info.lfthr[63].lim[0], cm.lf_info.lfthr[8].lim[0]), (4, 2));
        // Luma, vertical edges (level 20, scale 1) and horizontal edges (level 40, scale 2).
        assert_eq!(cm.lf_info.lvl[0][0][0][0][0], 21);
        assert_eq!(cm.lf_info.lvl[0][0][0][4], [19, 21]);
        assert_eq!(cm.lf_info.lvl[0][0][1][0][0], 42);
        assert_eq!(cm.lf_info.lvl[0][7][1][4], [38, 42]);
        // U has its level; V has none and keeps what it had.
        assert_eq!(cm.lf_info.lvl[1][3][1][1], [10, 12]);
        assert_eq!(cm.lf_info.lvl[2][0][0][1], [0, 0]);
        // A segment with its own level for the vertical edges of luma, and no deltas.
        cm.lf.mode_ref_delta_enabled = 0;
        cm.seg.enabled = 1;
        cm.seg.feature_mask[2] = 1 << 1;
        cm.seg.feature_data[2][1] = -30;
        loop_filter_frame_init(&mut cm);
        assert_eq!(cm.lf_info.lvl[0][2][0], [[0; 2]; 8]);
        assert_eq!(cm.lf_info.lvl[0][2][1], [[40; 2]; 8]);
        assert_eq!(cm.lf_info.lvl[0][1][0], [[20; 2]; 8]);
    }

    #[test]
    fn the_units_of_the_loop_restoration() {
        // SAFETY: all zero is a frame header.
        let mut cm: Box<AV1_COMMON> = unsafe { super::super::rav1d_frames::zeroed_box() };
        cm.superres_upscaled_width = 3840;
        cm.height = 2160;
        cm.seq_params.subsampling_x = 1;
        cm.seq_params.subsampling_y = 1;
        // SAFETY: all zero is the description of a plane without units.
        let mut rsi: RestorationInfo = unsafe { std::mem::zeroed() };
        rsi.restoration_unit_size = 256;
        alloc_restoration_struct(&cm, &mut rsi, false);
        // 3840 / 256 = 15; 2160 / 256 = 8.4, to the nearest.
        assert_eq!((rsi.horz_units_per_tile, rsi.vert_units_per_tile, rsi.units_per_tile), (15, 8, 120));
        alloc_restoration_struct(&cm, &mut rsi, true);
        assert_eq!((rsi.horz_units_per_tile, rsi.vert_units_per_tile), (8, 4));
        // A plane smaller than half a unit has one.
        cm.superres_upscaled_width = 100;
        cm.height = 100;
        alloc_restoration_struct(&cm, &mut rsi, false);
        assert_eq!(rsi.units_per_tile, 1);
    }
}
