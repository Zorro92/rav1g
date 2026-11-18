// SPDX-License-Identifier: BSD-2-Clause
//
// This source code is subject to the terms of the BSD 2 Clause License and the Alliance for Open Media
// Patent License 1.0. If the BSD 2 Clause License was not distributed with this source code in the LICENSE
// file, you can obtain it at www.aomedia.org/license/software. If the Alliance for Open Media Patent
// License 1.0 was not distributed with this source code in the PATENTS file, you can obtain it at
// www.aomedia.org/license/patent.

//! The records the decoder shares with the shaders, and the cursors of a tile.
//!
//! The parser writes these into buffers that the GPU stages read.
//!
//! - [`MB_MODE_INFO`], the mode info record of a block. The shaders read it as 32-bit words
//!   (`libav1/dx/shaders/mode_info.h`). The layout tests hold the fields against that header.
//! - [`tx_block_info_gpu`], the record of a transform block: the IDCT shaders load it as one `uint4`
//!   (`IdctBlockSize` of `idct_shader_common.h`).
//! - [`av1_tile_data`], the cursors of a tile into the buffers of a pipeline slot. No shader reads it; the
//!   sink of the parser (`rav1g-parse`) and the host (`rav1g`) both do.
//!
//! The size, alignment and field offsets of each record are checked at compile time.

use std::ffi::{c_int, c_uint, c_void};
use std::mem::{offset_of, zeroed};

use crate::bitstream::{
    BLOCK_SIZE, FILTER_INTRA_MODE, INTERINTRA_MODE, MOTION_MODE, MV_REFERENCE_FRAME, PARTITION_TYPE, PREDICTION_MODE,
    TX_SIZE, TransformationType, UV_PREDICTION_MODE, tran_low_t,
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
/// One block in the mode info pool, in the word order of `dx/shaders/mode_info.h`.
#[repr(C)]
#[derive(Default, Copy, Clone)]
pub struct MB_MODE_INFO {
    /// The six coefficients and four signed shear parameters of a local warp.
    pub wmmat: [i32; 6],
    pub warp_angles: [i16; 4],
    pub mv: [int_mv; 2],
    pub mi_row: u16,
    pub mi_col: u16,
    pub index_base: c_int,
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
    pub cdef_strength: i8,
    pub segment_id: i8,
    pub inter_tx_size: [u8; 16],
    pub delta_lf: [i8; 4],
    pub delta_lf_from_base: i8,
    pub cfl_alpha_idx: u8,
    pub cfl_alpha_signs: u8,
    pub interintra_wedge_index: u8,
    pub palette_size: [u8; 2],
    pub filter_intra_mode: FILTER_INTRA_MODE,
    pub flags: u8,
    pub compound_type: u8,
    pub wedge_index: u8,
    /// Vertical, then horizontal.
    pub interp_filters: [u8; 2],
}
impl MB_MODE_INFO {
    pub const USE_FILTER_INTRA: u8 = 1 << 0;
    pub const WARP_INVALID: u8 = 1 << 1;
    pub const MASK_TYPE: u8 = 1 << 2;
    pub const WEDGE_SIGN: u8 = 1 << 3;
}
const _: () = {
    assert!(size_of::<MB_MODE_INFO>() == 96);
    assert!(align_of::<MB_MODE_INFO>() == 4);
    assert!(offset_of!(MB_MODE_INFO, wmmat) == 0);
    assert!(offset_of!(MB_MODE_INFO, warp_angles) == 24);
    assert!(offset_of!(MB_MODE_INFO, mv) == 32);
    assert!(offset_of!(MB_MODE_INFO, mi_row) == 40);
    assert!(offset_of!(MB_MODE_INFO, mi_col) == 42);
    assert!(offset_of!(MB_MODE_INFO, index_base) == 44);
    assert!(offset_of!(MB_MODE_INFO, sb_type) == 48);
    assert!(offset_of!(MB_MODE_INFO, partition) == 49);
    assert!(offset_of!(MB_MODE_INFO, ref_frame) == 50);
    assert!(offset_of!(MB_MODE_INFO, mode) == 52);
    assert!(offset_of!(MB_MODE_INFO, uv_mode) == 53);
    assert!(offset_of!(MB_MODE_INFO, interintra_mode) == 54);
    assert!(offset_of!(MB_MODE_INFO, motion_mode) == 55);
    assert!(offset_of!(MB_MODE_INFO, use_wedge_interintra) == 56);
    assert!(offset_of!(MB_MODE_INFO, use_intrabc) == 57);
    assert!(offset_of!(MB_MODE_INFO, angle_delta) == 58);
    assert!(offset_of!(MB_MODE_INFO, tx_size) == 60);
    assert!(offset_of!(MB_MODE_INFO, skip) == 61);
    assert!(offset_of!(MB_MODE_INFO, cdef_strength) == 62);
    assert!(offset_of!(MB_MODE_INFO, segment_id) == 63);
    assert!(offset_of!(MB_MODE_INFO, inter_tx_size) == 64);
    assert!(offset_of!(MB_MODE_INFO, delta_lf) == 80);
    assert!(offset_of!(MB_MODE_INFO, delta_lf_from_base) == 84);
    assert!(offset_of!(MB_MODE_INFO, cfl_alpha_idx) == 85);
    assert!(offset_of!(MB_MODE_INFO, cfl_alpha_signs) == 86);
    assert!(offset_of!(MB_MODE_INFO, interintra_wedge_index) == 87);
    assert!(offset_of!(MB_MODE_INFO, palette_size) == 88);
    assert!(offset_of!(MB_MODE_INFO, filter_intra_mode) == 90);
    assert!(offset_of!(MB_MODE_INFO, flags) == 91);
    assert!(offset_of!(MB_MODE_INFO, compound_type) == 92);
    assert!(offset_of!(MB_MODE_INFO, wedge_index) == 93);
    assert!(offset_of!(MB_MODE_INFO, interp_filters) == 94);
};
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
    pub pred_block_bound: c_int,
    pub pred_warp_bound: c_int,
    pub intra_iter_max: c_int,
    pub intra_iter_max_uv: c_int,
    pub have_inter: c_int,
    pub gpu_pred_lists: c_int,
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
    pub have_recon: c_int,
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
    assert!(offset_of!(av1_tile_data, pred_block_bound) == 128);
    assert!(offset_of!(av1_tile_data, pred_warp_bound) == 132);
    assert!(offset_of!(av1_tile_data, intra_iter_max) == 136);
    assert!(offset_of!(av1_tile_data, intra_iter_max_uv) == 140);
    assert!(offset_of!(av1_tile_data, have_inter) == 144);
    assert!(offset_of!(av1_tile_data, gpu_pred_lists) == 148);
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
    assert!(offset_of!(av1_tile_data, have_recon) == 220);
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
