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
  // Output contexts (Av1Core::output_thread_data): what a show_existing_frame needs to copy a frame that was
  // decoded earlier into an output image. A command allocator and a fence of its own, and room for the film
  // grain constants and offsets; none of the buffers of a pipeline slot. Two, so that a second
  // show_existing_frame does not wait for the copy of the one before it.
  OutputThreadDataCount = 2,
  OutputCbSize = 32 * 1024,  // constants of one output copy: two film grain blocks, under 10 KB together
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

// Test flag -interfast (av1_set_inter_fast; docs/gpu-av1-decoder.md, "Inter prediction: breakdown and fast
// paths"). A reference fetch has a class by the fraction of its motion vector: 0 none (copy), 1 in x only,
// 2 in y only, 3 both. On a frame that takes the fast path the blocks of the lists CasualInter and
// CompoundAvrg are listed by class instead, in a block map of their own per tile (av1_tile_data::
// gen_block_map_fast), one list per class and block size:
//   class * InterSizesAllCommon + size                                        one reference
//   InterFastCompound + (class0 | class1 << 2) * InterSizesAllCommon + size   two references
// gen_pred_blocks_fast.hlsl places the records by the same numbers.
enum {
  InterFastCopy = 0,
  InterFastH = 1,
  InterFastV = 2,
  InterFast2D = 3,
  InterFastClasses = 4,
  InterFastCompound = InterFastClasses * (int)InterTypes::InterSizesAllCommon,
  InterFastLists = (InterFastClasses + InterFastClasses * InterFastClasses) * (int)InterTypes::InterSizesAllCommon,
  InterFastMapSize = InterFastLists + 16,  // ints of a tile's map: the lists and the end mark behind them
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

// Timestamps a command list can take. The stages need about twenty; the inter breakdown
// (av1_set_perf_detail) adds one per section, and in its mode 2 one per dispatch of the inter stage and of
// the reconstruction (at most AV1_PERF_INTER_SECTION_COUNT * AV1_PERF_INTER_LISTS, far fewer in a frame;
// with av1_set_inter_fast a frame can have 20 lists by class of 22 block sizes each).
enum { Av1PerfMaxMarks = 1280 };

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
  // Inter breakdown: 0 for a timestamp that closes a stage; else (section + 1) | ((list + 1) << 8) of the
  // section, and of the dispatch when the timestamp is behind one dispatch (list + 1 is 0 otherwise).
  uint16_t mark_sub[Av1PerfMaxMarks];
  int64_t t_submit;
  av1_perf_accum acc;
  // Inter breakdown of the frame of this command list: NULL until the slot records its first frame with
  // av1_set_perf_detail() on (malloc, freed with the decoder). detail_mode: the mode of the frame, 0 none.
  // detail_filled: the stage code has begun the record of this frame.
  av1_perf_inter_detail* detail;
  int detail_mode;
  int detail_filled;
};

// Inter breakdown: what the tile threads count for one tile while they push its blocks (av1_mi_push_block).
struct av1_perf_inter_tile {
  av1_perf_inter_work work[AV1_PERF_INTER_SECTION_COUNT];
  av1_perf_inter_blocks blocks;
};

// Test program flag -dumpstages (av1_set_stage_dump): one copy of a buffer behind a stage of the frame's
// command list.
enum { Av1StageDumpMax = 40 };
struct av1_stage_dump_entry {
  Microsoft::WRL::ComPtr<ID3D12Resource> readback;
  const char* stage;
  char* layout;  // kind 2: the text that describes the data (malloc), else NULL
  int kind;
  int bytes_per_sample;
  int bit_depth;
  size_t size;
  av1_stage_dump_plane planes[3];
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
  // The frame of this command list is output: it is shown and it is the frame of its temporal unit that
  // is output (av1_frame_is_output). Only such a frame takes an output image.
  int output_frame;

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
  // An output context, not a pipeline slot: only command_buffer, filmgrain_rand_offset and perf exist, and it
  // goes back to Av1Core::output_data_pool.
  int output_only;

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
  // -interfast: not 0 when this frame lists its blocks by motion vector class (no scaled reference): the mode
  // of av1_set_inter_fast, AV1_INTER_FAST_ON and the bits that keep the original shaders on those lists.
  int inter_fast;
  ScaleFactor scale_factors[8];
  // Entry LAST_FRAME..ALTREF_FRAME: a GLOBALMV or GLOBAL_GLOBALMV block of this frame is warped from that
  // reference. The reference has a global motion model libaom accepts and the size of the frame: a scaled
  // reference is never warped (av1_allow_warp of libaom, useWarp of the specification).
  int gm_warp[8];
  volatile int64_t perf_markers[16];
  av1_perf_td perf;

  // -dumpstages: the frame of this command list is dumped; its copies so far.
  int dump_active;
  int dump_count;
  int dump_is_sef;
  char dump_info[512];
  av1_stage_dump_entry dumps[Av1StageDumpMax];
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
  // -interfast: tiles the block maps have room for behind their pred_map_size ints (InterFastMapSize ints
  // each); 0 for a decoder that was created without the flag, which never takes the fast path.
  int inter_fast_tiles;
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
  // show_existing_frame: the output copy of a frame that is already decoded (av1_decode_sef).
  av1_frame_thread_data output_thread_data[OutputThreadDataCount];
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
  int max_bitdepth;  // the bit depth the frame buffers were created for: 8, or more than 8 (two bytes a sample)
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
  DataPtrQueueMT output_data_pool;
  GpuWorkItem gpu_item_pool_src[8];
  DataPtrQueueMT gpu_item_pool;
  DataPtrQueueMT gpu_waiting_queue;
  pthread_t gpu_thread;
  int tryhdr10x3;
  av1_perf_accum perf_cur;
  uint64_t perf_gpu_freq;
  uint64_t perf_gpu_last_end;
  // Inter breakdown (av1_set_perf_detail). perf_inter_tiles: the counters of the tiles of the frame the
  // tile threads are decoding, one per tile (malloc; NULL until the first such frame); perf_inter_mode: the
  // mode that frame was set up with. perf_detail_out: the completion thread's copy of a frame's breakdown,
  // which the perf callback gets (the slot may be recording its next frame by then).
  av1_perf_inter_tile* perf_inter_tiles;
  int perf_inter_tiles_cap;
  int perf_inter_mode;
  av1_perf_inter_detail* perf_detail_out;
} Av1Core;

void PutPerfMarker(av1_frame_thread_data* td, volatile int64_t* marker);
// -dumpstages (no-ops unless the frame of the current command list is to be dumped): record a copy of what a
// stage has just written. A frame buffer in its current layout; the residual buffer in the layout of the
// frame; back_buffer1 in the layout of the output image.
// |dims|: the buffer whose visible size the copy has, when |fb| is a back buffer that only has a layout.
void av1_stage_dump_fb(Av1Core* dec, const char* stage, const HwFrameBuffer* fb, const HwFrameBuffer* dims = NULL);
void av1_stage_dump_residuals(Av1Core* dec, const char* stage);
void av1_stage_dump_output(Av1Core* dec, const char* stage, const HwOutputImage* img, size_t size);
// -dumpstages, the buffers the stages read besides the frame (kind 2). The regions are copied one behind the
// other into one dump; |layout| is the text that describes them (copied). |uav|: the region's buffer is one
// the shaders write (unordered access state) and is a copy source for the length of the copy; a buffer the GPU
// only reads (upload or host memory, generic read state) is copied as it is.
struct av1_stage_dump_region {
  ID3D12Resource* src;
  size_t offset;
  size_t size;
  int uav;
};
int av1_stage_dump_active(Av1Core* dec);
void av1_stage_dump_data(Av1Core* dec, const char* stage, const char* layout, const av1_stage_dump_region* regions,
                         int count);
// The frame's mode info as the stages read it: the grid, and the records of the frame's tiles.
void av1_stage_dump_mode_info(Av1Core* dec, struct AV1Common* cm);
// The region of a constant block of the frame's command buffer.
av1_stage_dump_region av1_stage_dump_cb_region(Av1Core* dec, const ConstantBufferObject& cbo, size_t size);
// Timing instrumentation (no-ops unless a perf callback is installed).
void av1_perf_gpu_mark(Av1Core* dec, int stage);
// Inter breakdown (av1_set_perf_detail). av1_perf_inter_begin: the breakdown of the frame of the current
// command list, cleared, or NULL when the frame has none; the stage code fills its dispatch counters.
// av1_perf_inter_mark: a timestamp for |section| (and for its dispatch |list|, or -1), counted to |stage|.
av1_perf_inter_detail* av1_perf_inter_begin(Av1Core* dec);
void av1_perf_inter_mark(Av1Core* dec, int stage, int section, int list);
void av1_perf_add(Av1Core* dec, int id, int64_t start_ticks);
