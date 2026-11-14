// SPDX-License-Identifier: BSD-2-Clause
//
// This source code is subject to the terms of the BSD 2 Clause License and the Alliance for Open Media
// Patent License 1.0. If the BSD 2 Clause License was not distributed with this source code in the LICENSE
// file, you can obtain it at www.aomedia.org/license/software. If the Alliance for Open Media Patent
// License 1.0 was not distributed with this source code in the PATENTS file, you can obtain it at
// www.aomedia.org/license/patent.

//! The records the decoder shares with the shaders, and the cursors of a tile.
//!
//! The parser writes these into buffers that the GPU stages read, so their layouts are fixed: they are
//! the layouts `av1/common/blockd.h` and `mv.h` of libaom gave them in the C decoder, which the HLSL was
//! written against.
//!
//! - [`MB_MODE_INFO`], the mode info record of a block, with what it holds ([`PALETTE_MODE_INFO`],
//!   [`WarpedMotionParams`], [`INTERINTER_COMPOUND_DATA`], [`FILTER_INTRA_MODE_INFO`], [`int_mv`]). The
//!   shaders read it as `MB_MODE_INFO` of `libav1/dx/shaders/mode_info.h`, as four 32 bit words at a time
//!   (`ModeInfoSize` bytes a record). The tests of this crate hold the fields against that header
//!   (`layout_tests.rs`).
//! - [`tx_block_info_gpu`], the record of a transform block: the IDCT shaders load it as one `uint4`
//!   (`IdctBlockSize` of `idct_shader_common.h`).
//! - [`av1_tile_data`], the cursors of a tile into the buffers of a pipeline slot. No shader reads it; the
//!   sink of the parser (`rav1g-parse`) and the host (`rav1g`) both do.
//!
//! No C compiler sees these any more, so each layout is written down here: the size, the alignment and
//! the offset of every field, as the C compilers of the build had them (MSVC, clang-cl and clang for
//! Linux agreed) when the bindings were still generated from the headers. A change of a field that moves
//! anything fails the build.

use std::ffi::{c_int, c_uint, c_void};
use std::mem::{offset_of, zeroed};

use crate::bitstream::{
    BLOCK_SIZE, COMPOUND_TYPE, DIFFWTD_MASK_TYPE, FILTER_INTRA_MODE, INTERINTRA_MODE, InterpFilters, MOTION_MODE,
    MV_REFERENCE_FRAME, PARTITION_TYPE, PREDICTION_MODE, TX_SIZE, TransformationType, UV_PREDICTION_MODE, tran_low_t,
};

#[repr(C)]
#[derive(Debug, Default, Copy, Clone)]
pub struct mv {
    pub row: i16,
    pub col: i16,
}
const _: () = {
    assert!(size_of::<mv>() == 4);
    assert!(align_of::<mv>() == 2);
    assert!(offset_of!(mv, row) == 0);
    assert!(offset_of!(mv, col) == 2);
};
pub type MV = mv;
#[repr(C)]
#[derive(Copy, Clone)]
pub union int_mv {
    pub as_int: u32,
    pub as_mv: MV,
}
const _: () = {
    assert!(size_of::<int_mv>() == 4);
    assert!(align_of::<int_mv>() == 4);
    assert!(offset_of!(int_mv, as_int) == 0);
    assert!(offset_of!(int_mv, as_mv) == 0);
};
impl Default for int_mv {
    fn default() -> Self {
        // SAFETY: all zero is a value of every field.
        unsafe { zeroed() }
    }
}
#[repr(C)]
#[derive(Debug, Default, Copy, Clone)]
pub struct WarpedMotionParams {
    pub wmmat: [i32; 8],
    pub alpha: i16,
    pub beta: i16,
    pub gamma: i16,
    pub delta: i16,
    pub wmtype: TransformationType,
    pub invalid: i16,
}
const _: () = {
    assert!(size_of::<WarpedMotionParams>() == 44);
    assert!(align_of::<WarpedMotionParams>() == 4);
    assert!(offset_of!(WarpedMotionParams, wmmat) == 0);
    assert!(offset_of!(WarpedMotionParams, alpha) == 32);
    assert!(offset_of!(WarpedMotionParams, beta) == 34);
    assert!(offset_of!(WarpedMotionParams, gamma) == 36);
    assert!(offset_of!(WarpedMotionParams, delta) == 38);
    assert!(offset_of!(WarpedMotionParams, wmtype) == 40);
    assert!(offset_of!(WarpedMotionParams, invalid) == 42);
};
#[repr(C)]
#[derive(Debug, Default, Copy, Clone)]
pub struct PALETTE_MODE_INFO {
    pub palette_colors: [u16; 24],
    pub palette_size: [i16; 2],
}
const _: () = {
    assert!(size_of::<PALETTE_MODE_INFO>() == 52);
    assert!(align_of::<PALETTE_MODE_INFO>() == 2);
    assert!(offset_of!(PALETTE_MODE_INFO, palette_colors) == 0);
    assert!(offset_of!(PALETTE_MODE_INFO, palette_size) == 48);
};
#[repr(C)]
#[derive(Debug, Default, Copy, Clone)]
pub struct FILTER_INTRA_MODE_INFO {
    pub filter_intra_mode: FILTER_INTRA_MODE,
    pub use_filter_intra: u8,
}
const _: () = {
    assert!(size_of::<FILTER_INTRA_MODE_INFO>() == 2);
    assert!(align_of::<FILTER_INTRA_MODE_INFO>() == 1);
    assert!(offset_of!(FILTER_INTRA_MODE_INFO, filter_intra_mode) == 0);
    assert!(offset_of!(FILTER_INTRA_MODE_INFO, use_filter_intra) == 1);
};
#[repr(C)]
#[derive(Debug, Default, Copy, Clone)]
pub struct INTERINTER_COMPOUND_DATA {
    pub type_: COMPOUND_TYPE,
    pub mask_type: DIFFWTD_MASK_TYPE,
    pub wedge_index: c_int,
    pub wedge_sign: c_int,
}
const _: () = {
    assert!(size_of::<INTERINTER_COMPOUND_DATA>() == 16);
    assert!(align_of::<INTERINTER_COMPOUND_DATA>() == 4);
    assert!(offset_of!(INTERINTER_COMPOUND_DATA, type_) == 0);
    assert!(offset_of!(INTERINTER_COMPOUND_DATA, mask_type) == 4);
    assert!(offset_of!(INTERINTER_COMPOUND_DATA, wedge_index) == 8);
    assert!(offset_of!(INTERINTER_COMPOUND_DATA, wedge_sign) == 12);
};
#[repr(C)]
#[derive(Copy, Clone)]
pub struct MB_MODE_INFO {
    pub palette_mode_info: PALETTE_MODE_INFO,
    pub wm_params: WarpedMotionParams,
    pub interinter_comp: INTERINTER_COMPOUND_DATA,
    pub filter_intra_mode_info: FILTER_INTRA_MODE_INFO,
    pub mv: [int_mv; 2],
    pub interp_filters: InterpFilters,
    pub interintra_wedge_index: c_int,
    pub interintra_wedge_sign: c_int,
    pub current_qindex: c_int,
    pub delta_lf_from_base: c_int,
    pub delta_lf: [c_int; 4],
    pub mi_row: c_int,
    pub mi_col: c_int,
    pub index_base: c_int,
    pub num_proj_ref: c_int,
    pub cfl_alpha_idx: c_int,
    pub cfl_alpha_signs: c_int,
    pub comp_group_idx: i16,
    pub compound_idx: i16,
    pub sb_type: BLOCK_SIZE,
    pub partition: PARTITION_TYPE,
    pub ref_frame: [MV_REFERENCE_FRAME; 2],
    pub mode: PREDICTION_MODE,
    pub uv_mode: UV_PREDICTION_MODE,
    pub interintra_mode: INTERINTRA_MODE,
    pub motion_mode: MOTION_MODE,
    pub use_wedge_interintra: i8,
    pub use_intrabc: u8,
    pub angle_delta: [i8; 2],
    pub tx_size: TX_SIZE,
    pub skip: i8,
    pub skip_mode: i8,
    pub segment_id: i8,
    pub inter_tx_size: [u8; 16],
    pub seg_id_predicted: i8,
    pub cdef_strength: i8,
    pub ref_mv_idx: u8,
    pub reserved: i8,
}
const _: () = {
    assert!(size_of::<MB_MODE_INFO>() == 224);
    assert!(align_of::<MB_MODE_INFO>() == 4);
    assert!(offset_of!(MB_MODE_INFO, palette_mode_info) == 0);
    assert!(offset_of!(MB_MODE_INFO, wm_params) == 52);
    assert!(offset_of!(MB_MODE_INFO, interinter_comp) == 96);
    assert!(offset_of!(MB_MODE_INFO, filter_intra_mode_info) == 112);
    assert!(offset_of!(MB_MODE_INFO, mv) == 116);
    assert!(offset_of!(MB_MODE_INFO, interp_filters) == 124);
    assert!(offset_of!(MB_MODE_INFO, interintra_wedge_index) == 128);
    assert!(offset_of!(MB_MODE_INFO, interintra_wedge_sign) == 132);
    assert!(offset_of!(MB_MODE_INFO, current_qindex) == 136);
    assert!(offset_of!(MB_MODE_INFO, delta_lf_from_base) == 140);
    assert!(offset_of!(MB_MODE_INFO, delta_lf) == 144);
    assert!(offset_of!(MB_MODE_INFO, mi_row) == 160);
    assert!(offset_of!(MB_MODE_INFO, mi_col) == 164);
    assert!(offset_of!(MB_MODE_INFO, index_base) == 168);
    assert!(offset_of!(MB_MODE_INFO, num_proj_ref) == 172);
    assert!(offset_of!(MB_MODE_INFO, cfl_alpha_idx) == 176);
    assert!(offset_of!(MB_MODE_INFO, cfl_alpha_signs) == 180);
    assert!(offset_of!(MB_MODE_INFO, comp_group_idx) == 184);
    assert!(offset_of!(MB_MODE_INFO, compound_idx) == 186);
    assert!(offset_of!(MB_MODE_INFO, sb_type) == 188);
    assert!(offset_of!(MB_MODE_INFO, partition) == 189);
    assert!(offset_of!(MB_MODE_INFO, ref_frame) == 190);
    assert!(offset_of!(MB_MODE_INFO, mode) == 192);
    assert!(offset_of!(MB_MODE_INFO, uv_mode) == 193);
    assert!(offset_of!(MB_MODE_INFO, interintra_mode) == 194);
    assert!(offset_of!(MB_MODE_INFO, motion_mode) == 195);
    assert!(offset_of!(MB_MODE_INFO, use_wedge_interintra) == 196);
    assert!(offset_of!(MB_MODE_INFO, use_intrabc) == 197);
    assert!(offset_of!(MB_MODE_INFO, angle_delta) == 198);
    assert!(offset_of!(MB_MODE_INFO, tx_size) == 200);
    assert!(offset_of!(MB_MODE_INFO, skip) == 201);
    assert!(offset_of!(MB_MODE_INFO, skip_mode) == 202);
    assert!(offset_of!(MB_MODE_INFO, segment_id) == 203);
    assert!(offset_of!(MB_MODE_INFO, inter_tx_size) == 204);
    assert!(offset_of!(MB_MODE_INFO, seg_id_predicted) == 220);
    assert!(offset_of!(MB_MODE_INFO, cdef_strength) == 221);
    assert!(offset_of!(MB_MODE_INFO, ref_mv_idx) == 222);
    assert!(offset_of!(MB_MODE_INFO, reserved) == 223);
};
impl Default for MB_MODE_INFO {
    fn default() -> Self {
        // SAFETY: all zero is a value of every field.
        unsafe { zeroed() }
    }
}
#[repr(C)]
#[derive(Debug, Default, Copy, Clone)]
pub struct tx_block_info_gpu {
    pub flags: u32,
    pub input_offset: u32,
    pub output_pos: u32,
    pub sorting_idx: u32,
}
const _: () = {
    assert!(size_of::<tx_block_info_gpu>() == 16);
    assert!(align_of::<tx_block_info_gpu>() == 4);
    assert!(offset_of!(tx_block_info_gpu, flags) == 0);
    assert!(offset_of!(tx_block_info_gpu, input_offset) == 4);
    assert!(offset_of!(tx_block_info_gpu, output_pos) == 8);
    assert!(offset_of!(tx_block_info_gpu, sorting_idx) == 12);
};
#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct av1_tile_data {
    pub idct_blocks_host: *mut tx_block_info_gpu,
    pub dq_buffer_base: *mut tran_low_t,
    pub dq_buffer_offset: c_int,
    pub dq_buffer_ptr: c_int,
    pub dq_buffer_max: c_int,
    pub dq_buffer_end: c_int,
    pub dq_buffer_size: c_int,
    pub blocks_offset: c_int,
    pub idct_blocks_ptr: c_int,
    pub idct_blocks_sizes: [c_int; 21],
    pub block_count: c_int,
    pub block_count_wrp: c_int,
    pub intra_iter_max: c_int,
    pub intra_iter_max_uv: c_int,
    pub have_inter: c_int,
    pub gen_indexes: *mut c_uint,
    pub gen_index_ptr: c_int,
    pub gen_index_base: c_int,
    pub gen_block_map: *mut c_int,
    pub gen_block_map_wrp: *mut c_int,
    pub gen_intra_iter_y: c_int,
    pub gen_pred_map_max: c_int,
    pub gen_intra_iter_uv: c_int,
    pub gen_intra_max_iter: c_int,
    pub gen_intra_iter_set: c_int,
    pub gen_iter_clear_offset: c_int,
    pub gen_iter_clear_size: c_int,
    pub gen_block_map_offset: c_int,
    pub gen_block_warp_offset: c_int,
    pub gen_block_map_fast: *mut c_int,
    pub gen_block_fast_offset: c_int,
    pub mi_offset: c_int,
    pub mi_count: c_int,
    pub mi_col_start: c_int,
    pub mi_row_start: c_int,
    pub perf_inter: *mut c_void,
    pub wave_edges: *mut c_void,
    pub frame_data: *mut c_void,
}
const _: () = {
    assert!(size_of::<av1_tile_data>() == 280);
    assert!(align_of::<av1_tile_data>() == 8);
    assert!(offset_of!(av1_tile_data, idct_blocks_host) == 0);
    assert!(offset_of!(av1_tile_data, dq_buffer_base) == 8);
    assert!(offset_of!(av1_tile_data, dq_buffer_offset) == 16);
    assert!(offset_of!(av1_tile_data, dq_buffer_ptr) == 20);
    assert!(offset_of!(av1_tile_data, dq_buffer_max) == 24);
    assert!(offset_of!(av1_tile_data, dq_buffer_end) == 28);
    assert!(offset_of!(av1_tile_data, dq_buffer_size) == 32);
    assert!(offset_of!(av1_tile_data, blocks_offset) == 36);
    assert!(offset_of!(av1_tile_data, idct_blocks_ptr) == 40);
    assert!(offset_of!(av1_tile_data, idct_blocks_sizes) == 44);
    assert!(offset_of!(av1_tile_data, block_count) == 128);
    assert!(offset_of!(av1_tile_data, block_count_wrp) == 132);
    assert!(offset_of!(av1_tile_data, intra_iter_max) == 136);
    assert!(offset_of!(av1_tile_data, intra_iter_max_uv) == 140);
    assert!(offset_of!(av1_tile_data, have_inter) == 144);
    assert!(offset_of!(av1_tile_data, gen_indexes) == 152);
    assert!(offset_of!(av1_tile_data, gen_index_ptr) == 160);
    assert!(offset_of!(av1_tile_data, gen_index_base) == 164);
    assert!(offset_of!(av1_tile_data, gen_block_map) == 168);
    assert!(offset_of!(av1_tile_data, gen_block_map_wrp) == 176);
    assert!(offset_of!(av1_tile_data, gen_intra_iter_y) == 184);
    assert!(offset_of!(av1_tile_data, gen_pred_map_max) == 188);
    assert!(offset_of!(av1_tile_data, gen_intra_iter_uv) == 192);
    assert!(offset_of!(av1_tile_data, gen_intra_max_iter) == 196);
    assert!(offset_of!(av1_tile_data, gen_intra_iter_set) == 200);
    assert!(offset_of!(av1_tile_data, gen_iter_clear_offset) == 204);
    assert!(offset_of!(av1_tile_data, gen_iter_clear_size) == 208);
    assert!(offset_of!(av1_tile_data, gen_block_map_offset) == 212);
    assert!(offset_of!(av1_tile_data, gen_block_warp_offset) == 216);
    assert!(offset_of!(av1_tile_data, gen_block_map_fast) == 224);
    assert!(offset_of!(av1_tile_data, gen_block_fast_offset) == 232);
    assert!(offset_of!(av1_tile_data, mi_offset) == 236);
    assert!(offset_of!(av1_tile_data, mi_count) == 240);
    assert!(offset_of!(av1_tile_data, mi_col_start) == 244);
    assert!(offset_of!(av1_tile_data, mi_row_start) == 248);
    assert!(offset_of!(av1_tile_data, perf_inter) == 256);
    assert!(offset_of!(av1_tile_data, wave_edges) == 264);
    assert!(offset_of!(av1_tile_data, frame_data) == 272);
};
impl Default for av1_tile_data {
    fn default() -> Self {
        // SAFETY: all zero is a value of every field.
        unsafe { zeroed() }
    }
}
