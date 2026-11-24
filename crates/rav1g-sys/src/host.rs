// SPDX-License-Identifier: BSD-2-Clause
//
// This source code is subject to the terms of the BSD 2 Clause License and the Alliance for Open Media
// Patent License 1.0. If the BSD 2 Clause License was not distributed with this source code in the LICENSE
// file, you can obtain it at www.aomedia.org/license/software. If the Alliance for Open Media Patent
// License 1.0 was not distributed with this source code in the PATENTS file, you can obtain it at
// www.aomedia.org/license/patent.

//! The state of the decoder's host that had the shape of libaom's decoder structs.
//!
//! The Rust host was written file by file against the structs of the C decoder (`AV1Decoder`,
//! `AV1_COMMON`, `MACROBLOCKD`, `ThreadData`, the frame buffer descriptions of `dx/av1_core.h`, ...):
//! `host/rav1d_host.rs` fills the header of a frame into them from rav1d's headers, and the stages read
//! them. These are those structs with the fields the decoder uses and the names it uses them by. Nothing
//! outside the Rust of the decoder sees them, so no layout is kept: a field that nothing reads or writes
//! is not here, and the alignment the C asked for its SIMD code is gone. They stay `#[repr(C)]`, plain
//! data that is valid as all zero bytes, because the host creates them zeroed.
//!
//! The one exception is [`aom_film_grain_t`], which the host compares and copies as bytes: it has every
//! field, and its layout is held by constant assertions like those of `records.rs`.

use std::ffi::{c_char, c_int, c_uint, c_void};
use std::mem::offset_of;

use crate::api::{
    aom_bit_depth_t, aom_chroma_sample_position_t, aom_color_primaries_t, aom_color_range_t, aom_hdr_metadata_t,
    aom_matrix_coefficients_t, aom_transfer_characteristics_t, cfg_options_t,
};
use crate::bitstream::{BLOCK_SIZE, FRAME_TYPE, RestorationType};
use crate::records::{MB_MODE_INFO, WarpedMotionParams, av1_tile_data};

#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct PlaneInfo {
    pub stride: c_int,
    pub offset: c_int,
    pub res_stride: c_int,
    pub res_offset: c_int,
}
#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct HwFrameBuffer {
    pub base_offset: usize,
    pub size: usize,
    pub planes: [PlaneInfo; 3],
    pub width: c_int,
    pub height: c_int,
    pub hbd: c_int,
    pub y_crop_width: c_int,
    pub y_crop_height: c_int,
    pub uv_crop_width: c_int,
    pub uv_crop_height: c_int,
    pub frame_number: c_int,
    pub ref_cnt: c_int,
}
#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct HwOutputImage {
    pub fb_ptr: *mut u8,
    pub size: usize,
    pub planes: [PlaneInfo; 3],
    pub frame_number: c_int,
    pub y_crop_width: c_int,
    pub y_crop_height: c_int,
    pub uv_crop_width: c_int,
    pub uv_crop_height: c_int,
    pub hbd: c_int,
    pub monochrome: c_int,
    pub semi_planar: c_int,
    pub color_description_present: u8,
    pub color_primaries: u8,
    pub transfer_characteristics: u8,
    pub matrix_coefficients: u8,
    pub color_range: u8,
    pub chroma_sample_position: u8,
    pub hdr: aom_hdr_metadata_t,
    pub user_priv: *mut c_void,
    pub alloc_priv: *mut c_void,
    pub is_valid: c_int,
    /// `AV1_DISPLAY_TRANSFORM_*` of the display pass that wrote the image (0: none), and the size of the
    /// frame it read.
    pub display_transform: c_int,
    pub source_width: c_int,
    pub source_height: c_int,
    /// The source peak of the display pass's tone map (`aom_image::display_source_peak`).
    pub display_source_peak: f32,
}
#[repr(C)]
#[derive(Copy, Clone)]
pub struct yv12_buffer_config {
    pub widths: [c_int; 2],
    pub heights: [c_int; 2],
    pub crop_widths: [c_int; 2],
    pub crop_heights: [c_int; 2],
    pub strides: [c_int; 2],
    pub buffers: [*mut u8; 3],
    pub use_external_reference_buffers: c_int,
    pub buffer_alloc: *mut u8,
    pub buffer_alloc_sz: usize,
    pub border: c_int,
    pub frame_size: usize,
    pub subsampling_x: c_int,
    pub subsampling_y: c_int,
    pub bit_depth: c_uint,
    pub color_primaries: aom_color_primaries_t,
    pub transfer_characteristics: aom_transfer_characteristics_t,
    pub matrix_coefficients: aom_matrix_coefficients_t,
    pub monochrome: u8,
    pub chroma_sample_position: aom_chroma_sample_position_t,
    pub color_range: aom_color_range_t,
    pub flags: c_int,
    pub hw_buffer: *mut HwFrameBuffer,
    pub hw_show_image: *mut HwOutputImage,
}
pub type YV12_BUFFER_CONFIG = yv12_buffer_config;
#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct segmentation {
    pub enabled: u8,
    pub feature_data: [[i16; 8]; 8],
    pub feature_mask: [c_uint; 8],
}
pub type InterpKernel = [i16; 8];
#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct scale_factors {
    pub x_scale_fp: c_int,
    pub y_scale_fp: c_int,
    pub x_step_q4: c_int,
    pub y_step_q4: c_int,
}
#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct TileInfo {
    pub mi_row_start: c_int,
    pub mi_row_end: c_int,
    pub mi_col_start: c_int,
    pub mi_col_end: c_int,
    pub tile_row: c_int,
    pub tile_col: c_int,
}
#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct AV1PixelRect {
    pub left: c_int,
    pub top: c_int,
    pub right: c_int,
    pub bottom: c_int,
}
#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct buf_2d {
    pub buf: *mut u8,
    pub width: c_int,
    pub height: c_int,
    pub stride: c_int,
}
#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct macroblockd_plane {
    pub subsampling_x: c_int,
    pub subsampling_y: c_int,
    pub dst: buf_2d,
    pub width: u8,
    pub height: u8,
}
#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct WienerInfo {
    pub vfilter: InterpKernel,
    pub hfilter: InterpKernel,
}
#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct SgrprojInfo {
    pub ep: c_int,
    pub xqd: [c_int; 2],
}
#[repr(C)]
#[derive(Copy, Clone)]
pub struct macroblockd {
    pub plane: [macroblockd_plane; 3],
    pub tile: TileInfo,
    pub mi_stride: c_int,
    /// The entry of the block in the mode info grid; the entries of its neighbours are around it. Null
    /// when the parse threads have no grid (the GPU builds it): the block's record and the records
    /// around it are then in the four fields at the end.
    pub mi: *mut MiGridEntry,
    /// Where the logical mode info chunks are mapped during parse.
    pub mi_chunks: *const std::cell::Cell<*mut MB_MODE_INFO>,
    pub up_available: c_int,
    pub left_available: c_int,
    pub chroma_up_available: c_int,
    pub chroma_left_available: c_int,
    pub mb_to_right_edge: c_int,
    pub mb_to_bottom_edge: c_int,
    pub wiener_info: [WienerInfo; 3],
    pub sgrproj_info: [SgrprojInfo; 3],
    pub lossless: [c_int; 8],
    pub cur_frame_force_integer_mv: c_int,
    pub global_motion: *const WarpedMotionParams,
    pub current_qindex: c_int,
    pub delta_lf_from_base: c_int,
    pub delta_lf: [c_int; 4],
    pub cb_offset: [u16; 3],
    pub tile_data: *mut av1_tile_data,
    /// Without the grid (`mi` is null): the block's record.
    pub mi_record: *mut MB_MODE_INFO,
    /// Without the grid: the records of the blocks in the row of 4x4 units above the block, a record a
    /// unit, from the block's first column on.
    pub mi_above: *const *mut MB_MODE_INFO,
    /// Without the grid: the records of the blocks in the column of 4x4 units to the left of the block,
    /// a record a unit, by the row's place in a superblock row of 128x128 ([`MI_LEFT_EDGE`] entries: a
    /// row's entry is at its number modulo that).
    pub mi_left: *const *mut MB_MODE_INFO,
    /// Without the grid: the record of the 4x4 block above and to the left of a 4x4 block in an odd row
    /// and column, which is the first block of the 8x8 the two are in.
    pub mi_above_left: *mut MB_MODE_INFO,
}
pub type MACROBLOCKD = macroblockd;
/// The entries of [`macroblockd::mi_left`]: the 4x4 rows of a superblock of 128x128.
pub const MI_LEFT_EDGE: usize = 32;
#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct loopfilter {
    pub filter_level: [c_int; 2],
    pub filter_level_u: c_int,
    pub filter_level_v: c_int,
    pub sharpness_level: c_int,
    pub mode_ref_delta_enabled: u8,
    pub mode_ref_delta_update: u8,
    pub ref_deltas: [i8; 8],
    pub mode_deltas: [i8; 2],
    pub combine_vert_horz_lf: c_int,
}
#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct loop_filter_thresh {
    pub mblim: [u8; 16],
    pub lim: [u8; 16],
    pub hev_thr: [u8; 16],
}
#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct loop_filter_info_n {
    pub lfthr: [loop_filter_thresh; 64],
    pub lvl: [[[[[u8; 2]; 8]; 2]; 8]; 3],
}
#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct RestorationUnitInfo {
    pub restoration_type: RestorationType,
    pub wiener_info: WienerInfo,
    pub sgrproj_info: SgrprojInfo,
}
#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct RestorationInfo {
    pub frame_restoration_type: RestorationType,
    pub restoration_unit_size: c_int,
    pub units_per_tile: c_int,
    pub vert_units_per_tile: c_int,
    pub horz_units_per_tile: c_int,
    pub unit_info: *mut RestorationUnitInfo,
    pub optimized_lr: c_int,
}
#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct FilterFrameCtxt {
    pub rsi: *const RestorationInfo,
    pub tile_stripe0: c_int,
    pub ss_x: c_int,
    pub ss_y: c_int,
    pub highbd: c_int,
    pub bit_depth: c_int,
    pub data8: *mut u8,
    pub dst8: *mut u8,
    pub data_stride: c_int,
    pub dst_stride: c_int,
    pub tile_rect: AV1PixelRect,
}
#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct AV1LrStruct {
    pub ctxt: [FilterFrameCtxt; 3],
    pub frame: *mut YV12_BUFFER_CONFIG,
    pub dst: *mut YV12_BUFFER_CONFIG,
}
#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct aom_internal_error_info {
    pub detail: [c_char; 80],
}
#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct aom_film_grain_t {
    pub apply_grain: c_int,
    pub update_parameters: c_int,
    pub scaling_points_y: [[c_int; 2]; 14],
    pub num_y_points: c_int,
    pub scaling_points_cb: [[c_int; 2]; 10],
    pub num_cb_points: c_int,
    pub scaling_points_cr: [[c_int; 2]; 10],
    pub num_cr_points: c_int,
    pub scaling_shift: c_int,
    pub ar_coeff_lag: c_int,
    pub ar_coeffs_y: [c_int; 24],
    pub ar_coeffs_cb: [c_int; 25],
    pub ar_coeffs_cr: [c_int; 25],
    pub ar_coeff_shift: c_int,
    pub cb_mult: c_int,
    pub cb_luma_mult: c_int,
    pub cb_offset: c_int,
    pub cr_mult: c_int,
    pub cr_luma_mult: c_int,
    pub cr_offset: c_int,
    pub overlap_flag: c_int,
    pub clip_to_restricted_range: c_int,
    pub bit_depth: c_uint,
    pub chroma_scaling_from_luma: c_int,
    pub grain_scale_shift: c_int,
    pub random_seed: u16,
}
const _: () = {
    assert!(size_of::<aom_film_grain_t>() == 648);
    assert!(align_of::<aom_film_grain_t>() == 4);
    assert!(offset_of!(aom_film_grain_t, apply_grain) == 0);
    assert!(offset_of!(aom_film_grain_t, update_parameters) == 4);
    assert!(offset_of!(aom_film_grain_t, scaling_points_y) == 8);
    assert!(offset_of!(aom_film_grain_t, num_y_points) == 120);
    assert!(offset_of!(aom_film_grain_t, scaling_points_cb) == 124);
    assert!(offset_of!(aom_film_grain_t, num_cb_points) == 204);
    assert!(offset_of!(aom_film_grain_t, scaling_points_cr) == 208);
    assert!(offset_of!(aom_film_grain_t, num_cr_points) == 288);
    assert!(offset_of!(aom_film_grain_t, scaling_shift) == 292);
    assert!(offset_of!(aom_film_grain_t, ar_coeff_lag) == 296);
    assert!(offset_of!(aom_film_grain_t, ar_coeffs_y) == 300);
    assert!(offset_of!(aom_film_grain_t, ar_coeffs_cb) == 396);
    assert!(offset_of!(aom_film_grain_t, ar_coeffs_cr) == 496);
    assert!(offset_of!(aom_film_grain_t, ar_coeff_shift) == 596);
    assert!(offset_of!(aom_film_grain_t, cb_mult) == 600);
    assert!(offset_of!(aom_film_grain_t, cb_luma_mult) == 604);
    assert!(offset_of!(aom_film_grain_t, cb_offset) == 608);
    assert!(offset_of!(aom_film_grain_t, cr_mult) == 612);
    assert!(offset_of!(aom_film_grain_t, cr_luma_mult) == 616);
    assert!(offset_of!(aom_film_grain_t, cr_offset) == 620);
    assert!(offset_of!(aom_film_grain_t, overlap_flag) == 624);
    assert!(offset_of!(aom_film_grain_t, clip_to_restricted_range) == 628);
    assert!(offset_of!(aom_film_grain_t, bit_depth) == 632);
    assert!(offset_of!(aom_film_grain_t, chroma_scaling_from_luma) == 636);
    assert!(offset_of!(aom_film_grain_t, grain_scale_shift) == 640);
    assert!(offset_of!(aom_film_grain_t, random_seed) == 644);
};
#[repr(C)]
#[derive(Copy, Clone)]
pub struct RefCntBuffer {
    pub ref_count: c_int,
    pub order_hint: c_uint,
    pub seg: segmentation,
    pub mi_rows: c_int,
    pub mi_cols: c_int,
    pub width: c_int,
    pub height: c_int,
    pub global_motion: [WarpedMotionParams; 8],
    pub showable_frame: c_int,
    pub film_grain_params_present: u8,
    pub film_grain_params: aom_film_grain_t,
    pub buf: YV12_BUFFER_CONFIG,
    pub frame_type: FRAME_TYPE,
    pub ref_deltas: [i8; 8],
    pub mode_deltas: [i8; 2],
}
#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct CdefInfo {
    pub cdef_pri_damping: c_int,
    pub cdef_sec_damping: c_int,
    pub nb_cdef_strengths: c_int,
    pub cdef_strengths: [c_int; 16],
    pub cdef_uv_strengths: [c_int; 16],
    pub cdef_bits: c_int,
}
#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct DeltaQInfo {
    pub delta_q_present_flag: c_int,
    pub delta_lf_present_flag: c_int,
    pub delta_lf_multi: c_int,
}
#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct OrderHintInfo {
    pub enable_order_hint: c_int,
    pub order_hint_bits_minus_1: c_int,
    pub enable_dist_wtd_comp: c_int,
    pub enable_ref_frame_mvs: c_int,
}
#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct SequenceHeader {
    pub max_frame_width: c_int,
    pub max_frame_height: c_int,
    pub sb_size: BLOCK_SIZE,
    pub mib_size: c_int,
    pub mib_size_log2: c_int,
    pub order_hint_info: OrderHintInfo,
    pub force_integer_mv: u8,
    pub enable_intra_edge_filter: u8,
    pub enable_superres: u8,
    pub enable_cdef: u8,
    pub enable_restoration: u8,
    pub bit_depth: aom_bit_depth_t,
    pub use_highbitdepth: u8,
    pub monochrome: u8,
    pub color_description_present_flag: u8,
    pub color_primaries: aom_color_primaries_t,
    pub transfer_characteristics: aom_transfer_characteristics_t,
    pub matrix_coefficients: aom_matrix_coefficients_t,
    pub color_range: c_int,
    pub subsampling_x: c_int,
    pub subsampling_y: c_int,
    pub chroma_sample_position: aom_chroma_sample_position_t,
    pub film_grain_params_present: u8,
}
#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct CurrentFrame {
    pub frame_type: FRAME_TYPE,
    pub order_hint: c_uint,
    pub frame_number: c_uint,
    pub refresh_frame_flags: c_int,
}
#[repr(C)]
#[derive(Copy, Clone)]
pub struct AV1Common {
    pub current_frame: CurrentFrame,
    pub error: aom_internal_error_info,
    pub width: c_int,
    pub height: c_int,
    pub cur_frame: *mut RefCntBuffer,
    pub remapped_ref_idx: [c_int; 8],
    pub ref_scale_factors: [scale_factors; 8],
    pub ref_frame_map: [*mut RefCntBuffer; 8],
    pub show_frame: c_int,
    pub showable_frame: c_int,
    pub show_existing_frame: c_int,
    pub cur_frame_force_integer_mv: u8,
    pub allow_screen_content_tools: u8,
    pub allow_intrabc: c_int,
    pub mi_rows: c_int,
    pub mi_cols: c_int,
    pub mi_stride: c_int,
    pub base_qindex: c_int,
    pub mi_alloc_size: c_int,
    /// The pool of the mode info records: what the entries of the grid name records of.
    pub mi: *mut MB_MODE_INFO,
    pub mi_grid_base: *mut MiGridEntry,
    pub mi_grid_visible: *mut MiGridEntry,
    pub lf_info: loop_filter_info_n,
    pub superres_scale_denominator: u8,
    pub superres_upscaled_width: c_int,
    pub superres_upscaled_height: c_int,
    pub rst_info: [RestorationInfo; 3],
    pub rst_frame: YV12_BUFFER_CONFIG,
    pub lf: loopfilter,
    pub seg: segmentation,
    pub coded_lossless: c_int,
    pub all_lossless: c_int,
    pub tile_cols: c_int,
    pub tile_rows: c_int,
    pub tile_col_start_sb: [c_int; 65],
    pub tile_row_start_sb: [c_int; 65],
    pub skip_loop_filter: c_int,
    pub global_motion: [WarpedMotionParams; 8],
    pub film_grain_params: aom_film_grain_t,
    pub cdef_info: CdefInfo,
    pub delta_q_info: DeltaQInfo,
    pub seq_params: SequenceHeader,
    pub is_annexb: c_int,
    pub temporal_layer_id: c_int,
    pub spatial_layer_id: c_int,
    pub options: *const cfg_options_t,
    pub hdr_metadata: aom_hdr_metadata_t,
}
pub type AV1_COMMON = AV1Common;
#[repr(C)]
#[derive(Copy, Clone)]
pub struct ThreadData {
    pub xd: MACROBLOCKD,
    pub tile_data: *mut av1_tile_data,
    pub mi_pool: *mut MB_MODE_INFO,
    pub mi_count: i32,
    pub mi_count_max: i32,
}
#[repr(C)]
#[derive(Copy, Clone)]
pub struct AV1Decoder {
    pub mb: MACROBLOCKD,
    pub common: AV1_COMMON,
    /// The host of the GPU stages (`Av1Core` of the `rav1g` crate).
    pub gpu_decoder: *mut c_void,
    pub data: *const u8,
    pub user_priv: *mut c_void,
    pub lr_ctxt: AV1LrStruct,
    pub td: [ThreadData; 8],
    pub max_threads: c_int,
    pub operating_point: c_int,
    pub current_operating_point: c_int,
    pub output_all_layers: c_int,
    pub next_obu: *const u8,
    pub obus_end: *const u8,
    pub temporal_unit_end: *const u8,
}

// ---------------------------------------------------------------------------------------------------------
// The mode info grid: for every 4x4 unit of a frame the record of the block that covers it.
// ---------------------------------------------------------------------------------------------------------

/// An entry of the mode info grid: 0 for a unit without a block, else where the record of the block
/// starts in the pool of the mode info records, in bytes, plus [`MI_GRID_BASE`]. The shaders that read
/// the grid take the base off and divide by the size of a record (`cb_mi_addr_base`, `get_mi_index`).
/// libaom's grid, and the C++ host's, has the address of the record there, 8 bytes of which the
/// shaders read the low half; this is that word alone, with a base that does not depend on where the
/// pool is mapped.
pub type MiGridEntry = u32;
/// What an entry has above the place of its record in the pool: no record has the entry 0.
pub const MI_GRID_BASE: u32 = size_of::<MB_MODE_INFO>() as u32;
/// The bytes of an entry, as the shaders are told (`cb_mi_grid_entry`).
pub const MI_GRID_ENTRY_BYTES: u32 = size_of::<MiGridEntry>() as u32;
/// Mode info records in a pool chunk, one for each 4x4 unit of a 64x64 area.
pub const MI_POOL_CHUNK_RECORDS: usize = 256;

/// The entry of a record with logical index `index` in a pool smaller than 4 GiB.
#[inline]
pub fn mi_grid_entry(index: usize) -> MiGridEntry {
    (index as u32 + 1) * MI_GRID_BASE
}

/// Looks up a tile's record while its chunks can be in different buffers.
///
/// # Safety
/// The tile owns the chunk containing `entry`, and its mapping is set in `chunks`.
#[inline]
pub unsafe fn mi_grid_record_in_chunks(
    chunks: *const std::cell::Cell<*mut MB_MODE_INFO>,
    entry: MiGridEntry,
) -> *mut MB_MODE_INFO {
    if entry == 0 {
        std::ptr::null_mut()
    } else {
        let index = ((entry - MI_GRID_BASE) / MI_GRID_BASE) as usize;
        // SAFETY: the caller's contract.
        unsafe { (*chunks.add(index / MI_POOL_CHUNK_RECORDS)).get().add(index % MI_POOL_CHUNK_RECORDS) }
    }
}

/// The record an entry names in the pool at `pool`; null for the entry of a unit without a block.
///
/// # Safety
/// `entry` is 0 or an entry [`mi_grid_entry`] made for the pool at `pool`.
#[inline]
pub unsafe fn mi_grid_record(pool: *mut MB_MODE_INFO, entry: MiGridEntry) -> *mut MB_MODE_INFO {
    if entry == 0 {
        std::ptr::null_mut()
    } else {
        // SAFETY: the caller's contract.
        unsafe { pool.byte_add((entry - MI_GRID_BASE) as usize) }
    }
}
