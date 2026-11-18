// SPDX-License-Identifier: BSD-2-Clause
//
// This source code is subject to the terms of the BSD 2 Clause License and the Alliance for Open Media
// Patent License 1.0. If the BSD 2 Clause License was not distributed with this source code in the LICENSE
// file, you can obtain it at www.aomedia.org/license/software. If the Alliance for Open Media Patent
// License 1.0 was not distributed with this source code in the PATENTS file, you can obtain it at
// www.aomedia.org/license/patent.

//! The test instrumentation: the timing run, the stage dumps, the test switches, and the limits and
//! option bits of a decoder's configuration. `dx/av1_perf.h` of the C++ decoder.
//!
//! The values are those of `rav1g.h` (`capi.rs` of `rav1g` asserts the ones it has by name). The records
//! of the timing run and of a stage dump ([`av1_perf_frame`], [`av1_perf_inter_detail`],
//! [`av1_stage_dump`], [`av1_stage_dump_plane`]) reach the callbacks of `rav1g.h` as pointers, and the
//! test player reads them by layout, so their layouts are held by constant assertions like those of
//! `records.rs`.

use std::ffi::{c_char, c_int, c_void};
use std::mem::{offset_of, zeroed};

pub const AV1_MEMORY_GUARD_WAIT_MS: u32 = 2000;
pub const AV1_MEMORY_GUARD_SETTLE_MS: u32 = 300;
pub const AV1_LOW_MEMORY_SHARED_BACK_BUFFER: u32 = 1;
pub const AV1_LOW_MEMORY_MODE_INFO: u32 = 2;
pub const AV1_LOW_MEMORY_COEFS: u32 = 4;
pub const AV1_LOW_MEMORY_ALL: u32 = 7;
pub const AV1_LOW_MEMORY_NONE: u32 = 8;
pub const AV1_LOW_MEMORY_LATE_FRAME_BUFFER: u32 = 32;
pub const AV1_LOW_MEMORY_PARTS: u32 = 39;
pub const AV1_LOW_MEMORY_DEFAULT: u32 = 39;
pub const AV1_PARSER_OPTION_CLEAR_GRID: u32 = 32;
pub const AV1_PARSER_OPTION_TEST_PANIC: u32 = 64;
pub const AV1_PARSER_OPTION_TEST_POOL_WAIT: u32 = 128;
pub const AV1_PARSER_OPTION_GPU_CDEF_LISTS: u32 = 512;
pub const AV1_PARSER_OPTION_GPU_GRID_CHECK: u32 = 1024;
pub const AV1_PARSER_OPTION_GPU_GRID: u32 = 2048;
pub const AV1_FRAMES_IN_PARSE_MAX: u32 = 4;
pub const AV1_PERF_GPU_GEN_BLOCKS: c_int = 0;
pub const AV1_PERF_GPU_RESIDUAL_CLEAR: c_int = 1;
pub const AV1_PERF_GPU_IDCT_SORT: c_int = 2;
pub const AV1_PERF_GPU_IDCT: c_int = 3;
pub const AV1_PERF_GPU_INTER: c_int = 4;
pub const AV1_PERF_GPU_RECON: c_int = 5;
pub const AV1_PERF_GPU_INTRA: c_int = 6;
pub const AV1_PERF_GPU_LF_GEN: c_int = 7;
pub const AV1_PERF_GPU_LF_VERT: c_int = 8;
pub const AV1_PERF_GPU_LF_HORZ: c_int = 9;
pub const AV1_PERF_GPU_CDEF: c_int = 10;
pub const AV1_PERF_GPU_SUPERRES: c_int = 11;
pub const AV1_PERF_GPU_LOOP_REST: c_int = 12;
pub const AV1_PERF_GPU_EXT_BORDERS: c_int = 13;
pub const AV1_PERF_GPU_FILM_GRAIN: c_int = 14;
pub const AV1_PERF_GPU_COPY_PLANE: c_int = 15;
pub const AV1_PERF_GPU_OUTPUT_COPY: c_int = 16;
pub const AV1_PERF_GPU_SLOT_CLEAR: c_int = 17;
pub const AV1_PERF_GPU_CDEF_LISTS: c_int = 18;
pub const AV1_PERF_GPU_STAGE_COUNT: c_int = 19;
pub const AV1_PERF_GPU_BEGIN: c_int = 255;
pub const AV1_PERF_CPU_WAIT_SLOT: c_int = 0;
pub const AV1_PERF_CPU_WAIT_SEC_SLOT: c_int = 1;
pub const AV1_PERF_CPU_WAIT_FB: c_int = 2;
pub const AV1_PERF_CPU_WAIT_OUTPUT: c_int = 3;
pub const AV1_PERF_CPU_WAIT_IMAGE: c_int = 4;
pub const AV1_PERF_CPU_WAIT_ITEM: c_int = 5;
pub const AV1_PERF_CPU_SETUP: c_int = 6;
pub const AV1_PERF_CPU_MEMSET: c_int = 7;
pub const AV1_PERF_CPU_MEMSET_TILE: c_int = 8;
pub const AV1_PERF_CPU_ENTROPY_WALL: c_int = 9;
pub const AV1_PERF_CPU_ENTROPY_SUM: c_int = 10;
pub const AV1_PERF_CPU_RECORD: c_int = 11;
pub const AV1_PERF_CPU_REC_GEN_BLOCKS: c_int = 12;
pub const AV1_PERF_CPU_REC_IDCT: c_int = 13;
pub const AV1_PERF_CPU_REC_PRED: c_int = 14;
pub const AV1_PERF_CPU_REC_LOOPFILTER: c_int = 15;
pub const AV1_PERF_CPU_REC_CDEF_LR: c_int = 16;
pub const AV1_PERF_CPU_REC_BORDERS: c_int = 17;
pub const AV1_PERF_CPU_REC_COPY: c_int = 18;
pub const AV1_PERF_CPU_SUBMIT: c_int = 19;
pub const AV1_PERF_CPU_FRAME: c_int = 20;
pub const AV1_PERF_CPU_FENCE_WAIT: c_int = 21;
pub const AV1_PERF_CPU_GPU_LATENCY: c_int = 22;
pub const AV1_PERF_CPU_NOTIFY: c_int = 23;
pub const AV1_PERF_CPU_ASYNC_CLEAR: c_int = 24;
pub const AV1_PERF_CPU_SINK_LISTS: c_int = 25;
pub const AV1_PERF_CPU_SINK_CALLS: c_int = 26;
pub const AV1_PERF_CPU_SINK_GRID: c_int = 27;
pub const AV1_PERF_CPU_SINK_LISTS_INTRA: c_int = 28;
pub const AV1_PERF_CPU_COUNT: c_int = 29;
#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct av1_perf_frame {
    pub frame_number: c_int,
    pub show_frame: c_int,
    pub showable_frame: c_int,
    pub is_sef: c_int,
    pub intra_frame: c_int,
    pub used_sec_slot: c_int,
    pub intra_iters: c_int,
    pub intra_v2: c_int,
    pub intra_dispatches: c_int,
    pub intra_barriers: c_int,
    pub intra_groups: c_int,
    pub intra_passes: c_int,
    pub tile_count: c_int,
    pub gpu_valid: c_int,
    pub gpu_marks: c_int,
    pub gpu_stage_ms: [f64; 19],
    pub gpu_stage_hits: [c_int; 19],
    pub gpu_span_ms: f64,
    pub gpu_sum_ms: f64,
    pub gpu_gap_ms: f64,
    pub cpu_ms: [f64; 29],
    pub memset_bytes: i64,
    pub async_clear_bytes: i64,
    pub gpu_clear_bytes: i64,
    pub complete_ms: f64,
    pub inter: *const av1_perf_inter_detail,
}
const _: () = {
    assert!(size_of::<av1_perf_frame>() == 592);
    assert!(align_of::<av1_perf_frame>() == 8);
    assert!(offset_of!(av1_perf_frame, frame_number) == 0);
    assert!(offset_of!(av1_perf_frame, show_frame) == 4);
    assert!(offset_of!(av1_perf_frame, showable_frame) == 8);
    assert!(offset_of!(av1_perf_frame, is_sef) == 12);
    assert!(offset_of!(av1_perf_frame, intra_frame) == 16);
    assert!(offset_of!(av1_perf_frame, used_sec_slot) == 20);
    assert!(offset_of!(av1_perf_frame, intra_iters) == 24);
    assert!(offset_of!(av1_perf_frame, intra_v2) == 28);
    assert!(offset_of!(av1_perf_frame, intra_dispatches) == 32);
    assert!(offset_of!(av1_perf_frame, intra_barriers) == 36);
    assert!(offset_of!(av1_perf_frame, intra_groups) == 40);
    assert!(offset_of!(av1_perf_frame, intra_passes) == 44);
    assert!(offset_of!(av1_perf_frame, tile_count) == 48);
    assert!(offset_of!(av1_perf_frame, gpu_valid) == 52);
    assert!(offset_of!(av1_perf_frame, gpu_marks) == 56);
    assert!(offset_of!(av1_perf_frame, gpu_stage_ms) == 64);
    assert!(offset_of!(av1_perf_frame, gpu_stage_hits) == 216);
    assert!(offset_of!(av1_perf_frame, gpu_span_ms) == 296);
    assert!(offset_of!(av1_perf_frame, gpu_sum_ms) == 304);
    assert!(offset_of!(av1_perf_frame, gpu_gap_ms) == 312);
    assert!(offset_of!(av1_perf_frame, cpu_ms) == 320);
    assert!(offset_of!(av1_perf_frame, memset_bytes) == 552);
    assert!(offset_of!(av1_perf_frame, async_clear_bytes) == 560);
    assert!(offset_of!(av1_perf_frame, gpu_clear_bytes) == 568);
    assert!(offset_of!(av1_perf_frame, complete_ms) == 576);
    assert!(offset_of!(av1_perf_frame, inter) == 584);
};
impl Default for av1_perf_frame {
    fn default() -> Self {
        // SAFETY: all zero is a value of every field.
        unsafe { zeroed() }
    }
}
pub const AV1_PERF_INTER_WARP: c_int = 0;
pub const AV1_PERF_INTER_SINGLE: c_int = 1;
pub const AV1_PERF_INTER_SINGLE_COPY: c_int = 2;
pub const AV1_PERF_INTER_COMP_AVG: c_int = 6;
pub const AV1_PERF_INTER_COMP_AVG_FAST: c_int = 7;
pub const AV1_PERF_INTER_COMP_DIFF_Y: c_int = 23;
pub const AV1_PERF_INTER_COMP_MASKED: c_int = 24;
pub const AV1_PERF_INTER_COMP_WARP: c_int = 25;
pub const AV1_PERF_INTER_CHROMA_2X2: c_int = 26;
pub const AV1_PERF_INTER_SYNC0: c_int = 27;
pub const AV1_PERF_INTER_COMP_DIFF_UV: c_int = 28;
pub const AV1_PERF_INTER_COMP_WARP_DIFF_UV: c_int = 29;
pub const AV1_PERF_INTER_OBMC_ABOVE: c_int = 30;
pub const AV1_PERF_INTER_SYNC1: c_int = 31;
pub const AV1_PERF_INTER_OBMC_LEFT: c_int = 32;
pub const AV1_PERF_INTER_SYNC2: c_int = 33;
pub const AV1_PERF_INTER_RECON: c_int = 34;
pub const AV1_PERF_INTER_SECTION_COUNT: c_int = 35;
pub const AV1_PERF_INTER_LISTS: c_int = 36;
#[repr(C)]
#[derive(Debug, Default, Copy, Clone)]
pub struct av1_perf_inter_list {
    pub width: c_int,
    pub height: c_int,
    pub records: c_int,
    pub groups: c_int,
    pub threads: c_int,
    pub ms: f64,
}
const _: () = {
    assert!(size_of::<av1_perf_inter_list>() == 32);
    assert!(align_of::<av1_perf_inter_list>() == 8);
    assert!(offset_of!(av1_perf_inter_list, width) == 0);
    assert!(offset_of!(av1_perf_inter_list, height) == 4);
    assert!(offset_of!(av1_perf_inter_list, records) == 8);
    assert!(offset_of!(av1_perf_inter_list, groups) == 12);
    assert!(offset_of!(av1_perf_inter_list, threads) == 16);
    assert!(offset_of!(av1_perf_inter_list, ms) == 24);
};
#[repr(C)]
#[derive(Debug, Default, Copy, Clone)]
pub struct av1_perf_inter_work {
    pub blocks_y: i64,
    pub blocks_uv: i64,
    pub pixels_y: i64,
    pub pixels_uv: i64,
    pub ref_blocks: i64,
    pub footprint: i64,
    pub footprint_mv: i64,
}
const _: () = {
    assert!(size_of::<av1_perf_inter_work>() == 56);
    assert!(align_of::<av1_perf_inter_work>() == 8);
    assert!(offset_of!(av1_perf_inter_work, blocks_y) == 0);
    assert!(offset_of!(av1_perf_inter_work, blocks_uv) == 8);
    assert!(offset_of!(av1_perf_inter_work, pixels_y) == 16);
    assert!(offset_of!(av1_perf_inter_work, pixels_uv) == 24);
    assert!(offset_of!(av1_perf_inter_work, ref_blocks) == 32);
    assert!(offset_of!(av1_perf_inter_work, footprint) == 40);
    assert!(offset_of!(av1_perf_inter_work, footprint_mv) == 48);
};
#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct av1_perf_inter_section {
    pub ms: f64,
    pub timed: c_int,
    pub dispatches: c_int,
    pub groups: c_int,
    pub threads: i64,
    pub records: i64,
    pub pixels: i64,
    pub loaded_bytes: i64,
    pub work: av1_perf_inter_work,
    pub lists: [av1_perf_inter_list; 36],
}
const _: () = {
    assert!(size_of::<av1_perf_inter_section>() == 1264);
    assert!(align_of::<av1_perf_inter_section>() == 8);
    assert!(offset_of!(av1_perf_inter_section, ms) == 0);
    assert!(offset_of!(av1_perf_inter_section, timed) == 8);
    assert!(offset_of!(av1_perf_inter_section, dispatches) == 12);
    assert!(offset_of!(av1_perf_inter_section, groups) == 16);
    assert!(offset_of!(av1_perf_inter_section, threads) == 24);
    assert!(offset_of!(av1_perf_inter_section, records) == 32);
    assert!(offset_of!(av1_perf_inter_section, pixels) == 40);
    assert!(offset_of!(av1_perf_inter_section, loaded_bytes) == 48);
    assert!(offset_of!(av1_perf_inter_section, work) == 56);
    assert!(offset_of!(av1_perf_inter_section, lists) == 112);
};
impl Default for av1_perf_inter_section {
    fn default() -> Self {
        // SAFETY: all zero is a value of every field.
        unsafe { zeroed() }
    }
}
#[repr(C)]
#[derive(Debug, Default, Copy, Clone)]
pub struct av1_perf_inter_blocks {
    pub blocks: i64,
    pub area: i64,
    pub area_compound: i64,
    pub area_obmc: i64,
    pub area_warp: i64,
    pub area_wedge: i64,
    pub area_diffwtd: i64,
    pub area_interintra: i64,
    pub area_scaled: i64,
    pub ref_area: i64,
    pub ref_area_mv: [i64; 4],
    pub ref_area_filter: [[i64; 3]; 2],
}
const _: () = {
    assert!(size_of::<av1_perf_inter_blocks>() == 160);
    assert!(align_of::<av1_perf_inter_blocks>() == 8);
    assert!(offset_of!(av1_perf_inter_blocks, blocks) == 0);
    assert!(offset_of!(av1_perf_inter_blocks, area) == 8);
    assert!(offset_of!(av1_perf_inter_blocks, area_compound) == 16);
    assert!(offset_of!(av1_perf_inter_blocks, area_obmc) == 24);
    assert!(offset_of!(av1_perf_inter_blocks, area_warp) == 32);
    assert!(offset_of!(av1_perf_inter_blocks, area_wedge) == 40);
    assert!(offset_of!(av1_perf_inter_blocks, area_diffwtd) == 48);
    assert!(offset_of!(av1_perf_inter_blocks, area_interintra) == 56);
    assert!(offset_of!(av1_perf_inter_blocks, area_scaled) == 64);
    assert!(offset_of!(av1_perf_inter_blocks, ref_area) == 72);
    assert!(offset_of!(av1_perf_inter_blocks, ref_area_mv) == 80);
    assert!(offset_of!(av1_perf_inter_blocks, ref_area_filter) == 112);
};
#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct av1_perf_inter_detail {
    pub mode: c_int,
    pub has_inter: c_int,
    pub scaled: c_int,
    pub bytes_per_sample: c_int,
    pub barriers_added: c_int,
    pub marks: c_int,
    pub sections: [av1_perf_inter_section; 35],
    pub blocks: av1_perf_inter_blocks,
}
const _: () = {
    assert!(size_of::<av1_perf_inter_detail>() == 44424);
    assert!(align_of::<av1_perf_inter_detail>() == 8);
    assert!(offset_of!(av1_perf_inter_detail, mode) == 0);
    assert!(offset_of!(av1_perf_inter_detail, has_inter) == 4);
    assert!(offset_of!(av1_perf_inter_detail, scaled) == 8);
    assert!(offset_of!(av1_perf_inter_detail, bytes_per_sample) == 12);
    assert!(offset_of!(av1_perf_inter_detail, barriers_added) == 16);
    assert!(offset_of!(av1_perf_inter_detail, marks) == 20);
    assert!(offset_of!(av1_perf_inter_detail, sections) == 24);
    assert!(offset_of!(av1_perf_inter_detail, blocks) == 44264);
};
impl Default for av1_perf_inter_detail {
    fn default() -> Self {
        // SAFETY: all zero is a value of every field.
        unsafe { zeroed() }
    }
}
pub type av1_perf_frame_cb_fn_t = Option<unsafe extern "C" fn(priv_: *mut c_void, frame: *const av1_perf_frame)>;
pub const AV1_INTER_FAST_ON: c_int = 1;
pub const AV1_INTER_FAST_SINGLE_ORIGINAL: c_int = 2;
pub const AV1_INTER_FAST_COMPOUND_ORIGINAL: c_int = 4;
pub const AV1_INTER_FAST_LISTS_ONLY: c_int = 7;
pub const AV1_CLEARS_NOT_NEEDED: c_int = 0;
pub const AV1_CLEARS_GPU: c_int = 1;
pub const AV1_CLEARS_COMPLETION_THREAD: c_int = 2;
pub const AV1_CLEARS_DECODE_THREAD: c_int = 3;
pub const AV1_POISON_OFF: c_int = 0;
pub const AV1_POISON_MODE_INFO: c_int = 1;
pub const AV1_POISON_BUFFERS: c_int = 2;
#[repr(C)]
#[derive(Debug, Default, Copy, Clone)]
pub struct av1_stage_dump_plane {
    pub offset: u32,
    pub stride: u32,
    pub width: u32,
    pub height: u32,
}
const _: () = {
    assert!(size_of::<av1_stage_dump_plane>() == 16);
    assert!(align_of::<av1_stage_dump_plane>() == 4);
    assert!(offset_of!(av1_stage_dump_plane, offset) == 0);
    assert!(offset_of!(av1_stage_dump_plane, stride) == 4);
    assert!(offset_of!(av1_stage_dump_plane, width) == 8);
    assert!(offset_of!(av1_stage_dump_plane, height) == 12);
};
#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct av1_stage_dump {
    pub frame_number: c_int,
    pub index: c_int,
    pub stage: *const c_char,
    pub kind: c_int,
    pub bytes_per_sample: c_int,
    pub bit_depth: c_int,
    pub is_sef: c_int,
    pub info: *const c_char,
    pub planes: [av1_stage_dump_plane; 3],
    pub data: *const u8,
    pub size: usize,
    pub layout: *const c_char,
}
const _: () = {
    assert!(size_of::<av1_stage_dump>() == 112);
    assert!(align_of::<av1_stage_dump>() == 8);
    assert!(offset_of!(av1_stage_dump, frame_number) == 0);
    assert!(offset_of!(av1_stage_dump, index) == 4);
    assert!(offset_of!(av1_stage_dump, stage) == 8);
    assert!(offset_of!(av1_stage_dump, kind) == 16);
    assert!(offset_of!(av1_stage_dump, bytes_per_sample) == 20);
    assert!(offset_of!(av1_stage_dump, bit_depth) == 24);
    assert!(offset_of!(av1_stage_dump, is_sef) == 28);
    assert!(offset_of!(av1_stage_dump, info) == 32);
    assert!(offset_of!(av1_stage_dump, planes) == 40);
    assert!(offset_of!(av1_stage_dump, data) == 88);
    assert!(offset_of!(av1_stage_dump, size) == 96);
    assert!(offset_of!(av1_stage_dump, layout) == 104);
};
impl Default for av1_stage_dump {
    fn default() -> Self {
        // SAFETY: all zero is a value of every field.
        unsafe { zeroed() }
    }
}
pub type av1_stage_dump_cb_fn_t = Option<unsafe extern "C" fn(priv_: *mut c_void, dump: *const av1_stage_dump)>;
pub type av1_log_cb_fn_t = Option<unsafe extern "C" fn(priv_: *mut c_void, is_error: c_int, line: *const c_char)>;
