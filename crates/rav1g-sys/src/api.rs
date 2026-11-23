// SPDX-License-Identifier: BSD-2-Clause
//
// This source code is subject to the terms of the BSD 2 Clause License and the Alliance for Open Media
// Patent License 1.0. If the BSD 2 Clause License was not distributed with this source code in the LICENSE
// file, you can obtain it at www.aomedia.org/license/software. If the Alliance for Open Media Patent
// License 1.0 was not distributed with this source code in the PATENTS file, you can obtain it at
// www.aomedia.org/license/patent.

//! The decoder as `decoder.rs` and `capi.rs` of the `rav1g` crate drive it: the error codes, the
//! configuration of a decoder, the image a decoded frame is handed out as, and the callbacks for the
//! output buffers.
//!
//! These were the C interface of libaom's decoder (`aom/aom_codec.h`, `aom/aom_decoder.h`,
//! `aom/aom_image.h`) with the D3D12 additions of the C++ decoder. Both sides are Rust of the decoder
//! now and the interface for C is `rav1g.h`, so only the values count: the error codes are those of
//! `Rav1gError` (`lib.rs` and `capi.rs` of `rav1g` assert it), the colour enums are the code points of
//! the bitstream.

use std::ffi::{c_int, c_uint, c_void};
use std::mem::zeroed;

pub type aom_img_fmt = c_int;
pub type aom_img_fmt_t = aom_img_fmt;
pub type aom_color_primaries = c_int;
pub type aom_color_primaries_t = aom_color_primaries;
pub type aom_transfer_characteristics = c_int;
pub type aom_transfer_characteristics_t = aom_transfer_characteristics;
pub const AOM_CICP_MC_IDENTITY: aom_matrix_coefficients = 0;
pub type aom_matrix_coefficients = c_int;
pub type aom_matrix_coefficients_t = aom_matrix_coefficients;
pub type aom_color_range = c_int;
pub type aom_color_range_t = aom_color_range;
pub type aom_chroma_sample_position = c_int;
pub type aom_chroma_sample_position_t = aom_chroma_sample_position;
#[repr(C)]
#[derive(Debug, Default, Copy, Clone)]
pub struct aom_hdr_metadata {
    pub has_mdcv: u8,
    pub has_cll: u8,
    pub max_cll: u16,
    pub max_fall: u16,
    pub mdcv_primaries: [[u16; 2]; 3],
    pub mdcv_white_point: [u16; 2],
    pub mdcv_luminance_max: u32,
    pub mdcv_luminance_min: u32,
}
pub type aom_hdr_metadata_t = aom_hdr_metadata;
#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct aom_image {
    pub fmt: aom_img_fmt_t,
    pub cp: aom_color_primaries_t,
    pub tc: aom_transfer_characteristics_t,
    pub mc: aom_matrix_coefficients_t,
    pub monochrome: c_int,
    pub csp: aom_chroma_sample_position_t,
    pub range: aom_color_range_t,
    pub w: c_uint,
    pub h: c_uint,
    pub bit_depth: c_uint,
    pub d_w: c_uint,
    pub d_h: c_uint,
    pub r_w: c_uint,
    pub r_h: c_uint,
    pub x_chroma_shift: c_uint,
    pub y_chroma_shift: c_uint,
    pub planes: [*mut u8; 3],
    pub stride: [c_int; 3],
    pub sz: usize,
    pub bps: c_int,
    pub temporal_id: c_int,
    pub spatial_id: c_int,
    pub user_priv: *mut c_void,
    pub img_data: *mut u8,
    pub img_data_owner: c_int,
    pub self_allocd: c_int,
    pub fb_priv: *mut c_void,
    pub fb2_priv: *mut c_void,
    pub is_hdr10x3: c_int,
    pub semi_planar: c_int,
    pub color_description_present: c_int,
    pub hdr: aom_hdr_metadata_t,
    /// The display pass wrote the image (`av1_display_cfg`): `AV1_DISPLAY_TRANSFORM_*` (0: it did not), and
    /// the size of the frame it read. `cp` .. `csp` stay those of the stream.
    pub display_transform: c_int,
    pub source_w: c_uint,
    pub source_h: c_uint,
}
impl Default for aom_image {
    fn default() -> Self {
        // SAFETY: all zero is a value of every field.
        unsafe { zeroed() }
    }
}
pub type aom_image_t = aom_image;
pub const AOM_CODEC_OK: aom_codec_err_t = 0;
pub const AOM_CODEC_ERROR: aom_codec_err_t = 1;
pub const AOM_CODEC_MEM_ERROR: aom_codec_err_t = 2;
pub const AOM_CODEC_ABI_MISMATCH: aom_codec_err_t = 3;
pub const AOM_CODEC_INCAPABLE: aom_codec_err_t = 4;
pub const AOM_CODEC_UNSUP_BITSTREAM: aom_codec_err_t = 5;
pub const AOM_CODEC_UNSUP_FEATURE: aom_codec_err_t = 6;
pub const AOM_CODEC_CORRUPT_FRAME: aom_codec_err_t = 7;
pub const AOM_CODEC_INVALID_PARAM: aom_codec_err_t = 8;
pub const AOM_CODEC_LIST_END: aom_codec_err_t = 9;
pub const AOM_CODEC_NEEDS_RECONFIGURE: aom_codec_err_t = 10;
pub type aom_codec_err_t = c_int;
pub type aom_bit_depth = c_int;
pub type aom_bit_depth_t = aom_bit_depth;
#[repr(C)]
#[derive(Debug, Default, Copy, Clone)]
pub struct cfg_options {
    pub ext_partition: c_uint,
}
pub type cfg_options_t = cfg_options;
#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct av1_decoded_frame_buffer {
    pub dx12_buffer: *mut c_void,
    pub buffer_host_ptr: *mut c_void,
    pub buffer_size: usize,
    pub dx12_texture: [*mut c_void; 3],
    pub priv_: *mut c_void,
}
impl Default for av1_decoded_frame_buffer {
    fn default() -> Self {
        // SAFETY: all zero is a value of every field.
        unsafe { zeroed() }
    }
}
pub type av1_decoded_frame_buffer_t = av1_decoded_frame_buffer;
pub const fbt8bit: frame_buffer_type = 0;
pub const fbt10bit: frame_buffer_type = 1;
pub const fbt10x3: frame_buffer_type = 2;
pub const fbtNV12: frame_buffer_type = 3;
pub const fbtP010: frame_buffer_type = 4;
pub type frame_buffer_type = c_int;
pub type av1_get_decoded_buffer_cb_fn_t = Option<
    unsafe extern "C" fn(
        priv_: *mut c_void,
        buf_size: usize,
        w: u16,
        h: u16,
        fb_type: frame_buffer_type,
        fb: *mut av1_decoded_frame_buffer_t,
    ) -> c_int,
>;
pub type av1_release_decoded_buffer_cb_fn_t =
    Option<unsafe extern "C" fn(priv_: *mut c_void, buffer_priv: *mut c_void) -> c_int>;
pub type aom_notify_frame_ready_cb_fn_t = Option<unsafe extern "C" fn(priv_: *mut c_void, img: *mut c_void) -> c_int>;
#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct av1_out_buffers_cb {
    pub get_out_buffer_cb: av1_get_decoded_buffer_cb_fn_t,
    pub release_out_buffer_cb: av1_release_decoded_buffer_cb_fn_t,
    pub notify_frame_ready_cb: aom_notify_frame_ready_cb_fn_t,
    pub out_buffers_priv: *mut c_void,
}
impl Default for av1_out_buffers_cb {
    fn default() -> Self {
        // SAFETY: all zero is a value of every field.
        unsafe { zeroed() }
    }
}
pub type av1_out_buffers_cb_t = av1_out_buffers_cb;
pub type av1_memory_available_cb_fn_t = Option<unsafe extern "C" fn(priv_: *mut c_void) -> i64>;
/// The display pass of a decoder (`Rav1gSettings::display`, docs/stage-fusion.md), as `decoder.rs` resolves
/// it: width 0 for none. Every value is resolved (no zero defaults left), except the assumed colour code
/// points, where 0 is "none".
#[repr(C)]
#[derive(Debug, Default, Copy, Clone, PartialEq)]
pub struct av1_display_cfg {
    pub width: c_uint,
    pub height: c_uint,
    /// `AV1_DISPLAY_TRANSFORM_AUTO` or the one every frame takes.
    pub transform: c_uint,
    pub source_peak_nits: f32,
    pub target_white_nits: f32,
    /// 8 (NV12) or 10 (P010).
    pub depth: c_uint,
    pub assumed_primaries: u8,
    pub assumed_transfer: u8,
    pub assumed_matrix: u8,
    pub assumed_chroma_sample_position: u8,
}
/// `av1_display_cfg::transform` and `aom_image::display_transform`: what the display pass does to a frame.
pub const AV1_DISPLAY_TRANSFORM_AUTO: c_uint = 0;
pub const AV1_DISPLAY_TRANSFORM_SCALE: c_uint = 1;
pub const AV1_DISPLAY_TRANSFORM_GAMUT: c_uint = 2;
pub const AV1_DISPLAY_TRANSFORM_PQ: c_uint = 3;
pub const AV1_DISPLAY_TRANSFORM_HLG: c_uint = 4;
#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct aom_codec_dec_cfg {
    pub threads: c_uint,
    pub width: c_uint,
    pub height: c_uint,
    pub bitdepth: c_uint,
    pub allow_lowbitdepth: c_uint,
    pub cfg: cfg_options_t,
    pub host_memory: *mut c_void,
    pub host_size: usize,
    pub out_buffers_cb: av1_out_buffers_cb_t,
    pub dx12device: *mut c_void,
    pub dx12command_queue: *mut c_void,
    pub dxPsos: *mut c_void,
    pub tryHDR10x3: c_int,
    pub frame_slots: c_int,
    pub intra_v2: c_int,
    pub inter_fast: c_int,
    pub low_memory: c_int,
    pub parser_options: c_int,
    pub frames_in_parse: c_int,
    pub output_kind: c_int,
    pub shared_fence: c_int,
    pub memory_available_cb: av1_memory_available_cb_fn_t,
    pub memory_available_priv: *mut c_void,
    pub memory_headroom: usize,
    pub device_size: usize,
    pub device_size_deferred: usize,
    pub display: av1_display_cfg,
}
impl Default for aom_codec_dec_cfg {
    fn default() -> Self {
        // SAFETY: all zero is a value of every field.
        unsafe { zeroed() }
    }
}
pub type aom_codec_dec_cfg_t = aom_codec_dec_cfg;
