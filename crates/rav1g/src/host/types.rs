// SPDX-License-Identifier: BSD-2-Clause
//
// This source code is subject to the terms of the BSD 2 Clause License and the Alliance for Open Media
// Patent License 1.0. If the BSD 2 Clause License was not distributed with this source code in the LICENSE
// file, you can obtain it at www.aomedia.org/license/software. If the Alliance for Open Media Patent
// License 1.0 was not distributed with this source code in the PATENTS file, you can obtain it at
// www.aomedia.org/license/patent.

//! The structs of the host: the decoder (`Av1Core`), a pipeline slot (`av1_frame_thread_data`), the
//! allocator, the shader library, the queues, and the numbers of the block lists (`dx/types.h`,
//! `dx/av1_compute.h`, `dx/av1_memory.h`, `dx/av1_thread.h`, `dx/intra_v2_sched.h` of the C++ host).
//!
//! They are Rust's own since the last C++ file of the host left the cargo build: nothing outside this
//! crate reads them (the entropy decoder holds the decoder as a pointer it never looks behind). The names
//! and the fields are those of the C++ structs, and so are the layouts (`repr(C)`), for two reasons:
//!
//! - The decoder and the allocator lie in the host pool of the application, whose size the memory query
//!   reports. With the sizes of the C++ structs ([`CORE_BYTES`], [`ALLOCATOR_BYTES`]) a configuration takes
//!   the same host memory from both decoders, and the "MEMORY" lines of the two compare equal.
//! - A decoder is created zeroed, and all zero is a valid value of every field: no interface, no buffer,
//!   a count of 0.
//!
//! What was a COM pointer in C++ is the interface or the backend type that wraps it, or none; a lock or a
//! condition is the one of Windows that `aom_thread.h` made of a pthread one. What the entropy decoder
//! shares with the host (the frame buffer descriptions, the tile's cursors, the mode info) stays C and is
//! bound in `rav1g_sys::parser` and `rav1g_sys::shared`.

#![allow(non_camel_case_types, non_snake_case, non_upper_case_globals)]

use std::ffi::{c_char, c_int, c_long, c_uint, c_void};

use windows::Win32::Graphics::Direct3D12::{
    ID3D12CommandQueue, ID3D12Device, ID3D12PipelineState, ID3D12RootSignature,
};
use windows::Win32::System::Threading::{CONDITION_VARIABLE, CRITICAL_SECTION};

use crate::gpu::{CommandAllocator, CommandList, Fence, Readback, TimestampQueries};
use crate::sys::parser::{
    HwFrameBuffer, HwOutputImage, aom_notify_frame_ready_cb_fn_t, av1_get_decoded_buffer_cb_fn_t,
    av1_perf_inter_blocks, av1_perf_inter_detail, av1_perf_inter_work, av1_release_decoded_buffer_cb_fn_t,
    av1_stage_dump_plane, av1_tile_data,
};

/// Who reads and writes a buffer (`MemoryType` of `dx/av1_memory.h`; [`crate::gpu::MemoryKind`]).
pub type MemoryType = c_int;
/// A buffer of the device, as the pointer a buffer descriptor keeps (`ID3D12Resource *`).
pub type ID3D12Resource = c_void;
/// A lock, as `aom_thread.h` has it on Windows.
pub type pthread_mutex_t = CRITICAL_SECTION;
/// A condition, as `aom_thread.h` has it on Windows.
pub type pthread_cond_t = CONDITION_VARIABLE;

/// The size of the C++ `Av1Core`, which is what the decoder takes of the host pool.
pub const CORE_BYTES: usize = 67384;
/// The size of the C++ `av1_memory_allocator`.
pub const ALLOCATOR_BYTES: usize = 7296;
const _: () = assert!(size_of::<Av1Core>() == CORE_BYTES && size_of::<av1_memory_allocator>() == ALLOCATOR_BYTES);

#[repr(C)]
#[derive(Clone, Copy)]
pub struct GpuBufferObject {
    pub dev: *mut ID3D12Resource,
    pub host_ptr: *mut c_void,
    pub size: usize,
    pub max_size: usize,
    pub memtype: MemoryType,
    pub name: *const c_char,
    pub slot: c_int,
}

#[repr(C)]
pub struct dx_compute_context {
    pub device: Option<ID3D12Device>,
    pub queue: Option<ID3D12CommandQueue>,
    pub queue_direct: Option<ID3D12CommandQueue>,
    pub command_list: Option<CommandList>,
}

#[repr(C)]
pub struct av1_memory_allocator {
    pub vtable_: *const c_void,
    pub context: *mut dx_compute_context,
    pub dev_buffer_pool: [GpuBufferObject; 128],
    pub shader_obj_count: c_int,
    pub host_obj_count: c_int,
    pub dev_obj_count: c_int,
    pub dev_bytes: [usize; 6],
    pub dev_count: [c_int; 6],
    pub host_base_addr: *mut u8,
    pub host_max_size: usize,
    pub host_offset: usize,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct DataPtrQueue {
    pub m_element: [*mut c_void; 100],
    pub m_head: c_int,
    pub m_tail: c_int,
    pub m_QueueNotEmpty: c_int,
}

#[repr(C)]
pub struct DataPtrQueueMT {
    pub int_queue: DataPtrQueue,
    pub m_empty_cond: CONDITION_VARIABLE,
    pub m_mutex: CRITICAL_SECTION,
}

#[repr(C)]
pub struct ComputeShader {
    pub pso: Option<ID3D12PipelineState>,
    pub signaturePtr: Option<ID3D12RootSignature>,
    pub bytecode: *const u8,
    pub size: usize,
}

#[repr(C)]
pub struct bitdepth_dependent_shaders {
    pub inter_base: ComputeShader,
    pub inter_2x2: ComputeShader,
    pub inter_comp: ComputeShader,
    pub inter_comp_diff_y: ComputeShader,
    pub inter_comp_diff_uv: ComputeShader,
    pub inter_comp_masked: ComputeShader,
    pub inter_comp_2x2: ComputeShader,
    pub inter_warp: ComputeShader,
    pub inter_obmc_above: ComputeShader,
    pub inter_obmc_left: ComputeShader,
    pub inter_warp_comp: ComputeShader,
    pub intra_filter: ComputeShader,
    pub intra_main: ComputeShader,
    pub intra_main_opt: ComputeShader,
    pub reconstruct_block: ComputeShader,
    pub inter_ext_borders: ComputeShader,
    pub loopfilter_v: ComputeShader,
    pub loopfilter_h: ComputeShader,
    pub inter_scale: ComputeShader,
    pub inter_scale_2x2: ComputeShader,
    pub inter_scale_comp: ComputeShader,
    pub inter_scale_comp_diff_y: ComputeShader,
    pub inter_scale_comp_diff_uv: ComputeShader,
    pub inter_scale_comp_masked: ComputeShader,
    pub inter_scale_comp_2x2: ComputeShader,
    pub inter_scale_obmc_above: ComputeShader,
    pub inter_scale_obmc_left: ComputeShader,
    pub inter_scale_warp_comp: ComputeShader,
    pub inter_fast: ComputeShader,
    pub inter_comp_fast: ComputeShader,
}

#[repr(C)]
pub struct compute_shader_lib {
    pub shader_idct: [ComputeShader; 20],
    pub shader_idct_sort: ComputeShader,
    pub shader_loop_rest: ComputeShader,
    pub shader_cdef_filter: ComputeShader,
    pub shader_cdef_filter_v2: ComputeShader,
    pub shader_intra_v2: ComputeShader,
    pub shader_intra_v2_hbd: ComputeShader,
    pub shader_filmgrain_luma_gen: ComputeShader,
    pub shader_filmgrain_chroma_gen: ComputeShader,
    pub shader_filmgrain_filter: ComputeShader,
    pub shader_copy_plane: ComputeShader,
    pub shader_copy_plane_10bit10x3: ComputeShader,
    pub shader_fill_buffer: ComputeShader,
    pub shader_gen_pred_blocks: ComputeShader,
    pub shader_gen_pred_blocks_fast: ComputeShader,
    pub shader_gen_lf_vert: ComputeShader,
    pub shader_gen_lf_hor: ComputeShader,
    pub shader_upscale: ComputeShader,
    pub shaders_8bit: bitdepth_dependent_shaders,
    pub shaders_hbd: bitdepth_dependent_shaders,
    pub sig_idct: Option<ID3D12RootSignature>,
    pub sig_copy_plane: Option<ID3D12RootSignature>,
    pub sig_common111: Option<ID3D12RootSignature>,
    pub sig_common0102: Option<ID3D12RootSignature>,
    pub sig_common0110: Option<ID3D12RootSignature>,
    pub sig_pred_blocks: Option<ID3D12RootSignature>,
    pub sig_pred_blocks_fast: Option<ID3D12RootSignature>,
    pub sig_intra_pred: Option<ID3D12RootSignature>,
    pub sig_intra_v2: Option<ID3D12RootSignature>,
    pub sig_inter_pred: Option<ID3D12RootSignature>,
    pub sig_lf: Option<ID3D12RootSignature>,
    pub sig_lf_gen: Option<ID3D12RootSignature>,
    pub sig_loop_rest: Option<ID3D12RootSignature>,
    pub sig_cdef_filter: Option<ID3D12RootSignature>,
    pub sig_film_grain: Option<ID3D12RootSignature>,
    pub sig_upscale: Option<ID3D12RootSignature>,
    pub compliled_: c_int,
    pub threads: c_int,
    pub task_queue: DataPtrQueueMT,
    pub create_shader_threads: [*mut c_void; 6],
    pub create_shader_errors: c_int,
    pub task_cnt: c_int,
    pub create_shader_tasks: *mut c_void,
}

#[repr(C)]
pub struct ComputeCommandBuffer {
    pub allocator: Option<CommandAllocator>,
    pub fence: Option<Fence>,
    pub event: *mut c_void,
    pub cb_alloc: *mut GpuBufferObject,
    pub cb_ptr: usize,
    pub fence_value: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct av1_perf_accum {
    pub ticks: [i64; 25],
    pub memset_bytes: i64,
    pub t_frame_start: i64,
    pub t_record_start: i64,
    pub frame_number: c_int,
    pub show_frame: c_int,
    pub showable_frame: c_int,
    pub is_sef: c_int,
    pub intra_frame: c_int,
    pub used_sec_slot: c_int,
    pub intra_iters: c_int,
    pub intra_v2: c_int,
    pub intra_stats: [c_int; 4],
    pub tile_count: c_int,
}

#[repr(C)]
pub struct av1_perf_td {
    pub query_heap: Option<TimestampQueries>,
    pub readback: Option<Readback>,
    pub gpu_failed: c_int,
    pub gpu_active: c_int,
    pub collect: c_int,
    pub mark_count: c_int,
    pub mark_stage: [u8; 1280],
    pub mark_sub: [u16; 1280],
    pub t_submit: i64,
    pub acc: av1_perf_accum,
    pub detail: *mut av1_perf_inter_detail,
    pub detail_mode: c_int,
    pub detail_filled: c_int,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct av1_perf_inter_tile {
    pub work: [av1_perf_inter_work; 35],
    pub blocks: av1_perf_inter_blocks,
}

#[repr(C)]
pub struct av1_stage_dump_entry {
    pub readback: Option<Readback>,
    pub stage: *const c_char,
    pub layout: *mut c_char,
    pub kind: c_int,
    pub bytes_per_sample: c_int,
    pub bit_depth: c_int,
    pub size: usize,
    pub planes: [av1_stage_dump_plane; 3],
}

#[repr(C)]
pub struct av1_frame_thread_data {
    pub command_buffer: ComputeCommandBuffer,
    pub shaders: *mut bitdepth_dependent_shaders,
    pub mode_info_grid: *mut GpuBufferObject,
    pub idct_blocks_unordered: *mut GpuBufferObject,
    pub loop_rest_types: *mut GpuBufferObject,
    pub loop_rest_wiener: *mut GpuBufferObject,
    pub cdef_indexes: *mut GpuBufferObject,
    pub cdef_skips: *mut GpuBufferObject,
    pub filmgrain_rand_offset: *mut GpuBufferObject,
    pub gen_mi_block_indexes: *mut GpuBufferObject,
    pub gen_intra_inter_grid: *mut GpuBufferObject,
    pub gen_block_map: *mut GpuBufferObject,
    pub palette_buffer: *mut GpuBufferObject,
    pub intra_v2_buffer: *mut GpuBufferObject,
    pub intra_v2_regions: *mut c_uint,
    pub intra_v2_sched: IntraV2Sched,
    pub intra_v2_list_count: c_long,
    pub intra_v2: c_int,
    pub intra_v2_cells: c_int,
    pub intra_v2_cols: c_int,
    pub intra_v2_rows: c_int,
    pub intra_stats: [c_int; 4],
    pub gen_intra_iter_y: *mut c_int,
    pub gen_intra_iter_uv: *mut c_int,
    pub intra_iters: c_int,
    pub do_cdef: c_int,
    pub do_loop_rest: c_int,
    pub do_filmgrain: c_int,
    pub do_superres: c_int,
    pub is_hbd: c_int,
    pub output_frame: c_int,
    pub ext_idct_buffer: c_int,
    pub ext_coef_base: c_int,
    pub slot_index: c_int,
    pub iter_grid_stride: c_int,
    pub iter_grid_stride_uv: c_int,
    pub iter_grid_offset_uv: c_int,
    pub frame_number: c_int,
    pub bitdepth: c_int,
    pub thread_count: c_int,
    pub tile_count: c_int,
    pub mode_info_offset: c_int,
    pub mode_info_max: c_int,
    pub coef_buffer_offset: c_int,
    pub coef_buffer_size: c_int,
    pub tile_data: *mut av1_tile_data,
    pub frame_buffer: *mut HwFrameBuffer,
    pub back_buffer0: *mut HwFrameBuffer,
    pub dst_frame_buffer: *mut HwFrameBuffer,
    pub refs: [*mut HwFrameBuffer; 8],
    pub sec_thread_data: *mut av1_frame_thread_data,
    pub sec_data_mutex: CRITICAL_SECTION,
    pub output_only: c_int,
    pub async_clear: c_int,
    pub clear_pending: c_int,
    pub clear_mi_bytes: usize,
    pub clear_iter_bytes: usize,
    pub clean_mi_bytes: usize,
    pub clean_iter_bytes: usize,
    pub scale_enable: c_int,
    pub inter_fast: c_int,
    pub scale_factors: [ScaleFactor; 8],
    pub gm_warp: [c_int; 8],
    pub perf_markers: [i64; 16],
    pub perf: av1_perf_td,
    pub dump_active: c_int,
    pub dump_count: c_int,
    pub dump_is_sef: c_int,
    pub dump_info: [c_char; 512],
    pub dumps: [av1_stage_dump_entry; 40],
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct GpuWorkItem {
    pub data: *mut av1_frame_thread_data,
    pub image: *mut HwOutputImage,
}

#[repr(C)]
pub struct Av1Core {
    pub compute: dx_compute_context,
    pub memory: *mut av1_memory_allocator,
    pub shader_lib: *mut compute_shader_lib,
    pub cb_get_output_image: av1_get_decoded_buffer_cb_fn_t,
    pub cb_release_image: av1_release_decoded_buffer_cb_fn_t,
    pub cb_notify_frame_ready: aom_notify_frame_ready_cb_fn_t,
    pub image_alloc_priv: *mut c_void,
    pub ring_buf_host: *mut c_void,
    pub pbi_alloc: *mut c_void,
    pub buf_pool_alloc: *mut c_void,
    pub above_context_alloc: [*mut *mut c_void; 5],
    pub restoration_info_alloc: [*mut c_void; 3],
    pub tplmvs_alloc: *mut c_void,
    pub ring_buf_dev: *mut GpuBufferObject,
    pub idct_blocks: *mut GpuBufferObject,
    pub idct_residuals: *mut GpuBufferObject,
    pub frame_buffer_pool: *mut GpuBufferObject,
    pub inter_mask_lut: *mut GpuBufferObject,
    pub inter_warp_filter: *mut GpuBufferObject,
    pub mode_info_pool: *mut GpuBufferObject,
    pub idct_coefs: *mut GpuBufferObject,
    pub filmgrain_noise: *mut GpuBufferObject,
    pub filmgrain_gaus: *mut GpuBufferObject,
    pub filmgrain_random_luma: *mut GpuBufferObject,
    pub filmgrain_random_chroma: *mut GpuBufferObject,
    pub prediction_blocks: *mut GpuBufferObject,
    pub prediction_blocks_warp: *mut GpuBufferObject,
    pub loopfilter_blocks: *mut GpuBufferObject,
    pub fb_pool_alloc_ptr: usize,
    pub fb_pool_alloc_reset: usize,
    pub pred_map_size: c_int,
    pub inter_fast_tiles: c_int,
    pub block_count4x4: c_int,
    pub mode_info_shift: c_int,
    pub coef_count_num: c_int,
    pub shared_back_buffer: c_int,
    pub low_memory: c_int,
    pub palette_size: usize,
    pub memory_available_cb: ::std::option::Option<unsafe extern "C" fn(priv_: *mut c_void) -> i64>,
    pub memory_available_priv: *mut c_void,
    pub frame_thread_data: [av1_frame_thread_data; 4],
    pub frame_slots: c_int,
    pub output_thread_data: [av1_frame_thread_data; 2],
    pub curr_frame_data: *mut av1_frame_thread_data,
    pub frame_number: c_int,
    pub thread_count: c_int,
    pub corrupted_seq: c_int,
    pub back_buffer1: HwFrameBuffer,
    pub back_buffer0: HwFrameBuffer,
    pub fb_size: c_int,
    pub fb_offset: c_int,
    pub max_width: c_int,
    pub max_height: c_int,
    pub max_bitdepth: c_int,
    pub enable_superres: c_int,
    pub wedge_offsets: [[c_int; 2]; 22],
    pub fb_pool_src: [HwFrameBuffer; 14],
    pub image_pool_src: [HwOutputImage; 16],
    pub fb_pool_empty_cond: CONDITION_VARIABLE,
    pub fb_pool_mutex: CRITICAL_SECTION,
    pub fb_pool: DataPtrQueue,
    pub image_pool: DataPtrQueueMT,
    pub output_queue: DataPtrQueueMT,
    pub frame_data_pool: DataPtrQueueMT,
    pub output_data_pool: DataPtrQueueMT,
    pub gpu_item_pool_src: [GpuWorkItem; 8],
    pub gpu_item_pool: DataPtrQueueMT,
    pub gpu_waiting_queue: DataPtrQueueMT,
    pub gpu_thread: *mut c_void,
    pub tryhdr10x3: c_int,
    pub perf_cur: av1_perf_accum,
    pub perf_gpu_freq: u64,
    pub perf_gpu_last_end: u64,
    pub perf_inter_tiles: *mut av1_perf_inter_tile,
    pub perf_inter_tiles_cap: c_int,
    pub perf_inter_mode: c_int,
    pub perf_detail_out: *mut av1_perf_inter_detail,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct av1_stage_dump_region {
    pub src: *mut ID3D12Resource,
    pub offset: usize,
    pub size: usize,
    pub uav: c_int,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct IntraV2Sched {
    pub cells: *mut u32,
    pub regions: *mut u32,
    pub list: *mut u64,
    pub list_count: *mut c_long,
    pub map_stride: c_int,
    pub map_uv_offset: c_int,
    pub regions_stride: c_int,
    pub stamp: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct IntraV2Dispatch {
    pub first: c_int,
    pub count: c_int,
    pub step: u32,
    pub passes: u32,
    pub last_of_step: c_int,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct ScaleFactor {
    pub x_scale: c_int,
    pub x_step: c_int,
    pub y_scale: c_int,
    pub y_step: c_int,
}

pub const Av1PerfMaxMarks: c_int = 1280;
pub const Av1StageDumpMax: c_int = 40;
pub const BUFFERS_COUNT: c_int = 100;
pub const CasualInter: c_int = 1;
pub const CompoundAvrg: c_int = 2;
pub const CompoundDiff: c_int = 3;
pub const CompoundDiffUv: c_int = 6;
pub const CompoundDiffUvGlobalWarp: c_int = 7;
pub const CompoundGlobalWarp: c_int = 5;
pub const CompoundMasked: c_int = 4;
pub const DevMemPoolSize: c_int = 128;
pub const DeviceOnly: c_int = 0;
pub const DeviceOnlyConst: c_int = 4;
pub const DeviceUpload: c_int = 1;
pub const EntropyThreadCount: c_int = 8;
pub const FrameThreadDataCount: c_int = 4;
pub const FrameThreadDataDefault: c_int = 2;
pub const HostRW: c_int = 2;
pub const ImagePoolSize: c_int = 16;
pub const Inter2x2ArrOffset: c_int = 216;
pub const InterCountsAll: c_int = 219;
pub const InterFast2D: c_int = 3;
pub const InterFastClasses: c_int = 4;
pub const InterFastCompound: c_int = 96;
pub const InterFastLists: c_int = 480;
pub const InterFastMapSize: c_int = 496;
pub const InterSizesAllCommon: c_int = 24;
pub const IntraBlockOffset: c_int = 264;
pub const IntraSizes: c_int = 9;
pub const IntraTypeCount: c_int = 10;
pub const IntraV2MaxRegions: c_int = 256;
pub const MemoryTypeCount: c_int = 6;
pub const ObmcAbove: c_int = 8;
pub const ObmcLeft: c_int = 9;
pub const OutputCbSize: c_int = 32768;
pub const OutputThreadDataCount: c_int = 2;
pub const ReadBack: c_int = 3;
pub const ReconBlockOffset: c_int = 219;
pub const ReconstructBlockSizes: c_int = 36;
pub const RefFrameBufferCount: c_int = 12;
pub const RefFrameBufferCountSharedBack: c_int = 10;
pub const UploadUAV: c_int = 5;
pub const Warp: c_int = 0;
