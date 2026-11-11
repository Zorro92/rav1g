/*
 * Copyright 2020 Google LLC
 *
 */

/*
 * Copyright (c) 2020, Alliance for Open Media. All rights reserved
 *
 * This source code is subject to the terms of the BSD 2 Clause License and
 * the Alliance for Open Media Patent License 1.0. If the BSD 2 Clause License
 * was not distributed with this source code in the LICENSE file, you can
 * obtain it at www.aomedia.org/license/software. If the Alliance for Open
 * Media Patent License 1.0 was not distributed with this source code in the
 * PATENTS file, you can obtain it at www.aomedia.org/license/patent.
 */

#pragma once
#include "av1/common/blockd.h"
#include "dx/av1_compute.h"
#include "dx/av1_memory.h"
#include "dx/av1_thread.h"
#include "av1/common/onyxc_int.h"
#include "dx/av1_perf.h"
#include "dx/intra_v2_sched.h"

enum {
  UsePipeline = 0,
  PipelineSize = 2,
  // Pipeline slots (av1_frame_thread_data). The number in use is a decoder creation parameter
  // (aom_codec_dec_cfg_t::frame_slots, Av1Core::frame_slots); arrays are sized for the maximum.
  FrameThreadDataDefault = 2,
  FrameThreadDataCount = 4,
  // Frame buffers in the pool with the default number of slots (the count is derived at
  // resource_config_from_cfg() in av1_core.cpp). Every slot above the default adds one.
  RefFrameBufferCount = 12,
  // The same when the frames share one back buffer (AV1_LOW_MEMORY_SHARED_BACK_BUFFER): no frame takes its
  // back buffer from the pool, which is one buffer per frame in flight.
  RefFrameBufferCountSharedBack = 10,
  RefFrameBufferMax = RefFrameBufferCount + FrameThreadDataCount - FrameThreadDataDefault,
  EntropyThreadCount = 8,
  ImagePoolSize = 16,
};

struct InterSortingInfo {
  uint8_t bw_log2;
  uint8_t bh_log2;
  uint8_t type;
  uint8_t type_index;
  uint32_t index;
};

struct InterBlock {
  uint16_t x, y;
  uint32_t flags;
  MV mv0;
  MV mv1;
};

enum InterTypes {
  // warp;
  Warp = 0,

  CasualInter,
  CompoundAvrg,
  CompoundDiff,
  CompoundMasked,
  CompoundGlobalWarp,
  // Inter2x2, Inter2x2Comp here;
  // sync
  CompoundDiffUv,
  CompoundDiffUvGlobalWarp,
  ObmcAbove,
  // sync
  ObmcLeft,

  // 2x2 only
  Inter2x2,  //-> first pass
  Inter2x2Comp,

  Inter2x2CompP2,  //-> pass 2

  InterSizesAllCommon = 24,
  InterTypesNon2x2 = Inter2x2,
  Inter2x2ArrOffset = InterSizesAllCommon * (InterTypesNon2x2 - 1),
  Inter2x2Count = 3,
  InterCountsAll = Inter2x2ArrOffset + Inter2x2Count,

  CompoundTypeAvrg = 0,
  CompoundTypeMasked = 1,
  CompoundTypeDiffY = 2,
  CompoundTypeDiffUv = 3,
};

enum {
  IntraSizes = 9,                   // bw_log + bh_log = (0..4) + (0..4) = 0..8
  ReconstructBlockSizes = 36,       //(0..5)*(0..5)
  IntraTypeCount = IntraSizes + 1,  /// + filter_intra
  IntraBlockOffset = InterTypes::InterCountsAll + ReconstructBlockSizes + IntraSizes,
  ReconBlockOffset = InterTypes::InterCountsAll,
};

struct ScaleFactor {
  int x_scale;
  int x_step;
  int y_scale;
  int y_step;
};

struct ConstantBufferObject {
  void* host_ptr;
  D3D12_GPU_VIRTUAL_ADDRESS dev_address;
  ConstantBufferObject(void* ptr, D3D12_GPU_VIRTUAL_ADDRESS addr) : host_ptr(ptr), dev_address(addr) {}
};

struct ComputeCommandBuffer {
  Microsoft::WRL::ComPtr<ID3D12CommandAllocator> allocator;
  Microsoft::WRL::ComPtr<ID3D12Fence> fence;
  HANDLE event;

  GpuBufferObject* cb_alloc;
  size_t cb_ptr;
  uint32_t fence_value;

  ConstantBufferObject Alloc(size_t size) {
    size = (size + 255) & (~255);
    assert(cb_ptr + size < cb_alloc->size);
    size_t p = cb_ptr;
    cb_ptr += size;
    return ConstantBufferObject(reinterpret_cast<char*>(cb_alloc->host_ptr) + p,
                                cb_alloc->dev->GetGPUVirtualAddress() + p);
  }
  void Reset() { cb_ptr = 0; }
};

enum { Av1PerfMaxMarks = 32 };

// CPU-side timing of the frame currently being built (QPC ticks).
struct av1_perf_accum {
  volatile LONG64 ticks[AV1_PERF_CPU_COUNT];
  volatile LONG64 memset_bytes;
  int64_t t_frame_start;
  int64_t t_record_start;
  int frame_number;
  int show_frame;
  int showable_frame;
  int is_sef;
  int intra_frame;
  int used_sec_slot;
  int intra_iters;
  int intra_v2;
  int intra_stats[4];
  int tile_count;
};

// Per pipeline slot: GPU timestamp queries for the slot's command list and the
// CPU-side snapshot taken at submit.
struct av1_perf_td {
  Microsoft::WRL::ComPtr<ID3D12QueryHeap> query_heap;
  Microsoft::WRL::ComPtr<ID3D12Resource> readback;
  int gpu_failed;
  int gpu_active;
  int collect;
  int mark_count;
  uint8_t mark_stage[Av1PerfMaxMarks];
  int64_t t_submit;
  av1_perf_accum acc;
};

struct av1_frame_thread_data {
  ComputeCommandBuffer command_buffer;
  bitdepth_dependent_shaders* shaders;
  GpuBufferObject* mode_info_grid;
  GpuBufferObject* idct_blocks_unordered;

  GpuBufferObject* loop_rest_types;
  GpuBufferObject* loop_rest_wiener;
  GpuBufferObject* cdef_indexes;
  GpuBufferObject* cdef_skips;
  GpuBufferObject* filmgrain_rand_offset;

  GpuBufferObject* gen_mi_block_indexes;
  GpuBufferObject* gen_intra_inter_grid;
  GpuBufferObject* gen_block_map;

  // Pixels of the palette blocks of the frame, in the layout of a frame buffer. Created on the first frame of
  // the slot that allows screen content tools (no other frame has palette blocks); NULL until then.
  GpuBufferObject* palette_buffer;

  // Test player flag -intrav2: the schedule of the frame for intra_v2.hlsl (intra_v2_sched.h).
  // Both NULL unless the decoder was created for it (aom_codec_dec_cfg_t::intra_v2).
  GpuBufferObject* intra_v2_buffer;  // cell map, then region list
  unsigned int* intra_v2_regions;    // host only: current list entry and step of each region
  IntraV2Sched intra_v2_sched;
  volatile long intra_v2_list_count;
  int intra_v2;        // this frame takes the new path
  int intra_v2_cells;  // cells the buffer was sized for
  int intra_v2_cols;   // cell map layout of the frame before, in cells
  int intra_v2_rows;
  int intra_stats[4];  // intra stage: dispatches, UAV barriers, thread groups, groups times passes

  int* gen_intra_iter_y;
  int* gen_intra_iter_uv;
  int intra_iters;
  int do_cdef;
  int do_loop_rest;
  int do_filmgrain;
  int do_superres;
  int is_hbd;

  int ext_idct_buffer;
  int ext_coef_base;  // ext_idct_buffer: where the frame's coefficients start in idct_coefs, in coefficients
  int slot_index;
  int iter_grid_stride;
  int iter_grid_stride_uv;
  int iter_grid_offset_uv;
  int frame_number;
  int bitdepth;
  int thread_count;
  int tile_count;
  int mode_info_offset;
  int mode_info_max;
  int coef_buffer_offset;
  int coef_buffer_size;
  av1_tile_data* tile_data;
  HwFrameBuffer* frame_buffer;
  HwFrameBuffer* back_buffer0;  // the frame's back buffer when it came from the pool, else NULL
  HwFrameBuffer* dst_frame_buffer;
  HwFrameBuffer* refs[8];

  av1_frame_thread_data* sec_thread_data;
  pthread_mutex_t sec_data_mutex;

  // Test player flag -asyncclear (see av1_setup_frame). clear_*: what the frame that owns the slot cleared,
  // or found cleared, at setup; the completion thread clears the same bytes again once the frame's fence is
  // done. clean_*: bytes of the grids that are in the cleared state now. Written by the thread that holds the
  // slot: the completion thread before it returns the slot to the pool, the decode thread after it took it.
  int async_clear;          // the frame was set up with -asyncclear
  int clear_pending;        // the grids were written by a frame and not cleared since
  size_t clear_mi_bytes;    // mode_info_grid, from its start
  size_t clear_iter_bytes;  // gen_intra_inter_grid, from its start; 0: the frame did not clear it
  size_t clean_mi_bytes;
  size_t clean_iter_bytes;
  int scale_enable;
  ScaleFactor scale_factors[8];
  volatile int64_t perf_markers[16];
  av1_perf_td perf;
};

typedef struct {
  av1_frame_thread_data* data;
  HwOutputImage* image;
} GpuWorkItem;

typedef struct Av1Core {
  dx_compute_context compute;
  av1_memory_allocator* memory;
  compute_shader_lib* shader_lib;
  av1_get_decoded_buffer_cb_fn_t cb_get_output_image;
  av1_release_decoded_buffer_cb_fn_t cb_release_image;
  aom_notify_frame_ready_cb_fn_t cb_notify_frame_ready;
  void* image_alloc_priv;

  void* ring_buf_host;
  void* pbi_alloc;
  void* buf_pool_alloc;
  void** above_context_alloc[5];
  void* restoration_info_alloc[3];
  void* tplmvs_alloc;
  GpuBufferObject* ring_buf_dev;
  GpuBufferObject* idct_blocks;
  GpuBufferObject* idct_residuals;
  GpuBufferObject* frame_buffer_pool;
  GpuBufferObject* inter_mask_lut;
  GpuBufferObject* inter_warp_filter;
  GpuBufferObject* mode_info_pool;
  GpuBufferObject* idct_coefs;

  GpuBufferObject* filmgrain_noise;
  GpuBufferObject* filmgrain_gaus;
  GpuBufferObject* filmgrain_random_luma;
  GpuBufferObject* filmgrain_random_chroma;

  GpuBufferObject* prediction_blocks;
  GpuBufferObject* prediction_blocks_warp;
  GpuBufferObject* loopfilter_blocks;

  size_t fb_pool_alloc_ptr;
  size_t fb_pool_alloc_reset;

  int pred_map_size;
  int block_count4x4;
  // Shares of a slot in the mode info pool and in the coefficient buffer (AV1_LOW_MEMORY_MODE_INFO, _COEFS):
  // a tile of n 4x4 units has n >> mode_info_shift mode info entries and n * coef_count_num coefficients in
  // the share of a slot.
  int mode_info_shift;
  int coef_count_num;
  // AV1_LOW_MEMORY_SHARED_BACK_BUFFER: frames reconstruct into back_buffer0 instead of a pool buffer.
  int shared_back_buffer;
  int low_memory;
  size_t palette_size;  // bytes of a slot's palette buffer
  int64_t (*memory_available_cb)(void* priv);
  void* memory_available_priv;

  av1_frame_thread_data frame_thread_data[FrameThreadDataCount];
  int frame_slots;  // slots in use: the first frame_slots entries of frame_thread_data
  av1_frame_thread_data* curr_frame_data;
  int frame_number;
  int thread_count;
  int corrupted_seq;
  HwFrameBuffer back_buffer1;
  HwFrameBuffer back_buffer0;  // shared_back_buffer only
  int fb_size;
  int fb_offset;
  int max_width;   // the size the buffers were created for, a multiple of 128; larger frames are refused
  int max_height;
  int enable_superres;
  int wedge_offsets[BLOCK_SIZES_ALL][2];

  HwFrameBuffer fb_pool_src[RefFrameBufferMax];
  HwOutputImage image_pool_src[ImagePoolSize];
  pthread_cond_t fb_pool_empty_cond;
  pthread_mutex_t fb_pool_mutex;
  DataPtrQueue fb_pool;
  DataPtrQueueMT image_pool;
  DataPtrQueueMT output_queue;
  DataPtrQueueMT frame_data_pool;
  GpuWorkItem gpu_item_pool_src[8];
  DataPtrQueueMT gpu_item_pool;
  DataPtrQueueMT gpu_waiting_queue;
  pthread_t gpu_thread;
  int tryhdr10x3;
  av1_perf_accum perf_cur;
  uint64_t perf_gpu_freq;
  uint64_t perf_gpu_last_end;
} Av1Core;

void PutPerfMarker(av1_frame_thread_data* td, volatile int64_t* marker);
// Timing instrumentation (no-ops unless a perf callback is installed).
void av1_perf_gpu_mark(Av1Core* dec, int stage);
void av1_perf_add(Av1Core* dec, int id, int64_t start_ticks);
