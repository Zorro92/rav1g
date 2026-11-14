// SPDX-License-Identifier: BSD-2-Clause
//
// This source code is subject to the terms of the BSD 2 Clause License and the Alliance for Open Media
// Patent License 1.0. If the BSD 2 Clause License was not distributed with this source code in the LICENSE
// file, you can obtain it at www.aomedia.org/license/software. If the Alliance for Open Media Patent
// License 1.0 was not distributed with this source code in the PATENTS file, you can obtain it at
// www.aomedia.org/license/patent.

//! The structs of the host: the decoder (`Av1Core`), a pipeline slot (`av1_frame_thread_data`), the shader
//! library, and the numbers of the block lists (`dx/types.h`, `dx/av1_compute.h`, `dx/av1_thread.h`,
//! `dx/intra_v2_sched.h` of the C++ host, whose names they keep).
//!
//! They are Rust's own: nothing outside this crate reads them. The entropy decoder holds the decoder as a
//! pointer it never looks behind (`struct Av1Core *` of `dx/av1_core.h`), so the decoder is an allocation
//! of this crate ([`Av1Core::new`]) with the queues, the locks and the counters as Rust types, and its
//! layout is whatever the compiler makes of it. What the entropy decoder shares with the host (the frame
//! buffer descriptions, the tile's cursors, the mode info) stays C, is bound in `rav1g_sys::parser` and
//! `rav1g_sys::shared`, and is held against the C compiler by the layout tests there.
//!
//! A decoder starts zeroed except for the fields that are Rust values with a state of their own (the
//! queues, the pool of frame buffers, the locks, the vector of the tile counters): all zero is a valid
//! value of every other field, no interface, no buffer, a count of 0. What was a COM pointer in C++ is the
//! interface or the backend type that wraps it, or none.

#![allow(non_camel_case_types, non_snake_case, non_upper_case_globals)]

use std::ffi::{c_char, c_int, c_long, c_uint, c_void};
use std::sync::Mutex;
use std::thread::JoinHandle;

use windows::Win32::Graphics::Direct3D12::{
    ID3D12CommandQueue, ID3D12Device, ID3D12PipelineState, ID3D12RootSignature,
};

use super::blocks::WaveEdges;
use super::memory::Allocator;
use super::thread::{FramePool, Queue};
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
    pub create_shader_errors: c_int,
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
    /// One counter per CPU timer (`AV1_PERF_CPU_*` of `dx/av1_perf.h`).
    pub ticks: [i64; crate::sys::AV1_PERF_CPU_COUNT as usize],
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
    /// The inter breakdown of the slot's frame (-gputimedetail), from the first frame recorded with it.
    pub detail: Option<Box<av1_perf_inter_detail>>,
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
    /// Held while the frame takes its second slot: the decode thread or a tile thread.
    pub sec_data_mutex: Mutex<()>,
    pub output_only: c_int,
    /// What the frame on the slot does about the clears of the slot's grids (`AV1_CLEARS_*` of
    /// `dx/av1_perf.h`): the switch as the frame's setup found it.
    pub clears: c_int,
    /// With `AV1_CLEARS_GPU`: the frame's command list ends with the clears (`clear_mi_bytes`,
    /// `clear_iter_bytes`).
    pub gpu_cleared: c_int,
    pub clear_pending: c_int,
    pub clear_mi_bytes: usize,
    pub clear_iter_bytes: usize,
    pub clean_mi_bytes: usize,
    pub clean_iter_bytes: usize,
    /// The frames whose parse has written the mode info grid of the slot since its last clear: their
    /// width, height and row length in 4x4 units, and the bytes that clear covered. Every unit outside
    /// such a frame is still null. All 0: not known, the next frame clears the grid. Only kept for a
    /// decoder that keeps the grid (`Av1Core::keep_mi_grid`).
    pub kept_grid: [c_int; 3],
    pub kept_grid_bytes: usize,
    /// The frame keeps its intra wavefront in the edges of its tiles and not in the wavefront grid
    /// (`AV1_CLEARS_NOT_NEEDED`): the edges, for the tile threads, which each write those of their tile
    /// and the columns of their tile in the rows above (`setup_tile`). The memory is `wave`'s.
    pub wave_edges: c_int,
    pub wave_tiles: *mut WaveEdges,
    pub wave_above: *mut c_int,
    pub wave_above_end: *mut i64,
    /// The columns of luma a row of tiles has in the rows above; chroma has half of them behind those.
    pub wave_cols: usize,
    /// The stamp of the frame in the edges: the number of the frame among those that kept their
    /// wavefront on this slot, shifted left by 32 (`blocks::WavePlane::stamp`).
    pub wave_stamp: i64,
    pub wave: WaveStore,
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

/// The memory of the edges of the intra wavefront of the frames of a slot (`blocks::WaveEdges`). It grows
/// to what the largest frame of the slot needs and is never cleared: an entry that is read was written
/// by the frame that reads it.
pub struct WaveStore {
    /// The edges of each tile.
    pub tiles: Vec<WaveEdges>,
    /// The rows above the tiles and where their decoded units end: for every row of tiles the columns of
    /// luma, then those of chroma.
    pub above: Vec<c_int>,
    pub above_end: Vec<i64>,
    /// The frames that kept their wavefront here.
    pub frames: i64,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct GpuWorkItem {
    pub data: *mut av1_frame_thread_data,
    pub image: *mut HwOutputImage,
}

/// Work items: one per frame in flight (at most `FrameThreadDataCount`, each holds a slot) and one per
/// show_existing_frame copy in flight (at most `OutputThreadDataCount`, each holds an output context).
pub const GpuItemPoolSize: usize = 8;
/// The most frame buffers a decoder has: `RefFrameBufferCount` and one for each slot above two.
pub const FrameBufferPoolSize: usize = 14;

pub struct Av1Core {
    pub compute: dx_compute_context,
    /// The allocator, in an allocation of its own (the decoder keeps pointers to its buffer descriptors);
    /// null until the creation has made it.
    pub memory: *mut Allocator,
    pub shader_lib: *mut compute_shader_lib,
    pub cb_get_output_image: av1_get_decoded_buffer_cb_fn_t,
    pub cb_release_image: av1_release_decoded_buffer_cb_fn_t,
    pub cb_notify_frame_ready: aom_notify_frame_ready_cb_fn_t,
    pub image_alloc_priv: *mut c_void,
    pub pbi_alloc: *mut c_void,
    pub buf_pool_alloc: *mut c_void,
    pub above_context_alloc: [*mut *mut c_void; 5],
    pub restoration_info_alloc: [*mut c_void; 3],
    pub tplmvs_alloc: *mut c_void,
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
    pub back_buffer1: HwFrameBuffer,
    pub back_buffer0: HwFrameBuffer,
    pub fb_size: c_int,
    pub fb_offset: c_int,
    pub max_width: c_int,
    pub max_height: c_int,
    pub max_bitdepth: c_int,
    pub enable_superres: c_int,
    pub wedge_offsets: [[c_int; 2]; 22],
    pub fb_pool_src: [HwFrameBuffer; FrameBufferPoolSize],
    pub image_pool_src: [HwOutputImage; ImagePoolSize as usize],
    /// The frame buffers nothing counts on; its lock guards the reference counts of all of them.
    pub fb_pool: FramePool,
    /// Output images without a frame.
    pub image_pool: Queue<*mut HwOutputImage>,
    /// Finished frames of an application that takes them from the decode call (`get_output_frame`).
    pub output_queue: Queue<*mut HwOutputImage>,
    /// The pipeline slots that are free.
    pub frame_data_pool: Queue<*mut av1_frame_thread_data>,
    /// The output contexts that are free.
    pub output_data_pool: Queue<*mut av1_frame_thread_data>,
    pub gpu_item_pool_src: [GpuWorkItem; GpuItemPoolSize],
    /// Work items without work.
    pub gpu_item_pool: Queue<*mut GpuWorkItem>,
    /// The frames in flight, for the completion thread, in the order of their submission; null ends the
    /// thread (the drain).
    pub gpu_waiting_queue: Queue<*mut GpuWorkItem>,
    /// The completion thread, while it runs.
    pub gpu_thread: Option<JoinHandle<()>>,
    pub tryhdr10x3: c_int,
    pub perf_cur: av1_perf_accum,
    pub perf_gpu_freq: u64,
    pub perf_gpu_last_end: u64,
    /// The counters of the tile threads for the inter breakdown (-gputimedetail), one set per tile of
    /// the frame that is being decoded. Each tile thread writes the counters of its tile through
    /// `perf_inter_tiles_base`, which the frame setup takes from the vector; the decode thread reads the
    /// vector when the tiles are done.
    pub perf_inter_tiles: Vec<av1_perf_inter_tile>,
    pub perf_inter_tiles_base: *mut av1_perf_inter_tile,
    pub perf_inter_tiles_count: c_int,
    pub perf_inter_mode: c_int,
    /// The inter breakdown as the timing callback gets it: the completion thread's copy.
    pub perf_detail_out: Option<Box<av1_perf_inter_detail>>,
    /// The parser of phase 2 (`aom_codec_dec_cfg_t::parser`), made by the first decode call of a decoder
    /// that was created for it; none in a decoder whose parser is the entropy decoder of libaom.
    pub rav1d: Option<Box<super::rav1d_host::Rav1dHost>>,
    /// The threads the parser may use for the tiles of a frame (`aom_codec_dec_cfg_t::threads`).
    pub parser_threads: c_int,
    /// The switches of that parser (`aom_codec_dec_cfg_t::parser_options`).
    pub parser_options: c_int,
    /// The mode info grid of a slot is not cleared for a frame of the size of the frames before it on
    /// the slot (`rav1g_setup_frame_begin`): a decoder whose parser is rav1d's, unless its switches say
    /// otherwise (`AV1_PARSER_OPTION_CLEAR_GRID`).
    pub keep_mi_grid: c_int,
    /// The mode info grid and the wavefront grid of every slot are buffers a command list may write:
    /// the decoder was created while the clears were the GPU's (`AV1_CLEARS_GPU`). Only then does a
    /// frame end its command list with them.
    pub gpu_clear_grids: c_int,
}

impl Av1Core {
    /// A decoder with nothing in it: no device, no buffers, every count 0, the queues and the pool of
    /// frame buffers empty. `None` when the system has no memory for it.
    pub(crate) fn new() -> Option<Box<Av1Core>> {
        let layout = std::alloc::Layout::new::<Av1Core>();
        // SAFETY: the layout has a size. All zero is a valid value of every field but the ones written
        // here before the box is made: pointers, numbers, arrays of them, `Option`s of interfaces and of
        // boxes (none), and `gpu_thread` (none: written to make that sure).
        unsafe {
            let dec = std::alloc::alloc_zeroed(layout).cast::<Av1Core>();
            if dec.is_null() {
                return None;
            }
            (&raw mut (*dec).fb_pool).write(FramePool::with_capacity(FrameBufferPoolSize));
            (&raw mut (*dec).image_pool).write(Queue::with_capacity(ImagePoolSize as usize));
            (&raw mut (*dec).output_queue).write(Queue::with_capacity(ImagePoolSize as usize));
            (&raw mut (*dec).frame_data_pool).write(Queue::with_capacity(FrameThreadDataCount as usize));
            (&raw mut (*dec).output_data_pool).write(Queue::with_capacity(OutputThreadDataCount as usize));
            (&raw mut (*dec).gpu_item_pool).write(Queue::with_capacity(GpuItemPoolSize));
            // The work items of the frames in flight and the item that ends the thread.
            (&raw mut (*dec).gpu_waiting_queue).write(Queue::with_capacity(GpuItemPoolSize + 1));
            (&raw mut (*dec).gpu_thread).write(None);
            (&raw mut (*dec).perf_inter_tiles).write(Vec::new());
            (&raw mut (*dec).perf_detail_out).write(None);
            (&raw mut (*dec).rav1d).write(None);
            let no_edges = || WaveStore { tiles: Vec::new(), above: Vec::new(), above_end: Vec::new(), frames: 0 };
            for slot in 0..FrameThreadDataCount as usize {
                (&raw mut (*dec).frame_thread_data[slot].sec_data_mutex).write(Mutex::new(()));
                (&raw mut (*dec).frame_thread_data[slot].perf.detail).write(None);
                (&raw mut (*dec).frame_thread_data[slot].wave).write(no_edges());
            }
            for context in 0..OutputThreadDataCount as usize {
                (&raw mut (*dec).output_thread_data[context].sec_data_mutex).write(Mutex::new(()));
                (&raw mut (*dec).output_thread_data[context].perf.detail).write(None);
                (&raw mut (*dec).output_thread_data[context].wave).write(no_edges());
            }
            Some(Box::from_raw(dec))
        }
    }

    /// What a decoder takes of the heap of the process besides its host pool: this struct, the
    /// allocator with its buffer descriptors, and the storage of the queues.
    pub(crate) const fn own_bytes() -> usize {
        size_of::<Av1Core>()
            + size_of::<Allocator>()
            + size_of::<*mut c_void>()
                * (FrameBufferPoolSize
                    + 2 * ImagePoolSize as usize
                    + FrameThreadDataCount as usize
                    + OutputThreadDataCount as usize
                    + 2 * GpuItemPoolSize
                    + 1)
    }
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
