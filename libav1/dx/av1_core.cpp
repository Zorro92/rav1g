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

#include "dx/av1_core.h"
#include "dx/av1_memory.h"
#include "dx/av1_compute.h"
#include <assert.h>
#include <stdio.h>
#include <new>
#include "dx/types.h"
#include "av1/decoder/decoder.h"
#include "av1\common\scan.h"
#include "av1\common\idct.h"
#include "av1\common\filter.h"
#include "av1/common/reconinter.h"
#include "av1/common/warped_motion.h"
#include "av1/common/reconintra.h"
#include "aom_dsp/intrapred_common.h"

enum {
  IdctBlockSize = 16,
  IdctCoefCountDenum = 4 * 4 + 2 * 2 * 2,
  IdctCoefCountNum = 17,  //~71%
  IdctCoefCountNumLow = 12,  // AV1_LOW_MEMORY_COEFS: half; two shares are exactly the worst case
  TileSbSizeThreshold = 2,

  FbYStripe = 256,
  FbUvStripe = 128,
};

int av1_postprocess_copy_output(Av1Core *dec, AV1_COMMON *cm);
void av1_loopfilter_gpu(Av1Core *dec, AV1_COMMON *cm, MACROBLOCKD *xd);
void av1_cdef_looprestoration(Av1Core *dec, AV1_COMMON *cm, void *lr_ctxt);
void av1_prediction_run_all(Av1Core *dec, AV1_COMMON *cm, TileInfo *tile);
void av1_idct_run(Av1Core *dec);
void av1_inter_ext_borders(Av1Core *dec, AV1_COMMON *cm);
void av1_mi_push_block(AV1Decoder *pbi, AV1_COMMON *cm, MACROBLOCKD *xd);
void av1_prediction_gen_blocks(AV1Decoder *pbi, Av1Core *dec);

static THREADFN gpu_thread_hook(void *pdata);

// ---------------------------------------------------------------------------
// Timing instrumentation for the test player. Everything below is inert until
// av1_set_perf_callback() installs a callback; it never changes decode output.
// ---------------------------------------------------------------------------
static av1_perf_frame_cb_fn_t g_perf_cb = NULL;
static void *g_perf_priv = NULL;

extern "C" void av1_set_perf_callback(av1_perf_frame_cb_fn_t cb, void *priv) {
  g_perf_priv = priv;
  g_perf_cb = cb;
}

static inline int perf_enabled() { return g_perf_cb != NULL; }

// Test player flag -gputimedetail (av1_perf.h): the inter prediction breakdown. 0 off, 1 timestamps behind
// the sections of the inter stage and work counters, 2 also a UAV barrier and a timestamp behind every
// dispatch. Nothing of it exists without a perf callback.
static int g_perf_detail = 0;

extern "C" void av1_set_perf_detail(int mode) { g_perf_detail = mode < 0 ? 0 : mode > 2 ? 2 : mode; }

// Test flag -interfast (dx/av1_perf.h, av1_set_inter_fast; dx/types.h, InterFastLists): inter blocks listed
// and predicted by the fraction of their motion vectors. A frame takes the path while this is set, in a
// decoder that was created with room for the lists by class in its block maps
// (aom_codec_dec_cfg_t::inter_fast), unless it has a scaled reference (av1_setup_frame).
static int g_inter_fast = 0;

// |mode|: 0 off, or AV1_INTER_FAST_ON with the bits that keep the original shader on the lists by class.
extern "C" void av1_set_inter_fast(int mode) { g_inter_fast = (mode & AV1_INTER_FAST_ON) ? (mode & 7) : 0; }

static inline int64_t perf_now() {
  LARGE_INTEGER t;
  QueryPerformanceCounter(&t);
  return t.QuadPart;
}

extern "C" int64_t av1_perf_ticks(void) { return perf_enabled() ? perf_now() : 0; }

void av1_perf_add(Av1Core *dec, int id, int64_t start_ticks) {
  if (!start_ticks) return;
  InterlockedExchangeAdd64(&dec->perf_cur.ticks[id], perf_now() - start_ticks);
}

extern "C" void av1_perf_cpu_add(AV1Decoder *pbi, int id, int64_t start_ticks) {
  if (!start_ticks || !pbi || !pbi->gpu_decoder) return;
  av1_perf_add(pbi->gpu_decoder, id, start_ticks);
}

// Adds the time since |start| to |id| and returns the new lap start.
static int64_t perf_lap(Av1Core *dec, int id, int64_t start_ticks) {
  if (!start_ticks) return 0;
  const int64_t now = perf_now();
  InterlockedExchangeAdd64(&dec->perf_cur.ticks[id], now - start_ticks);
  return now;
}

static void perf_add_memset(Av1Core *dec, int id, int64_t start_ticks, size_t bytes) {
  if (!start_ticks) return;
  InterlockedExchangeAdd64(&dec->perf_cur.ticks[id], perf_now() - start_ticks);
  InterlockedExchangeAdd64(&dec->perf_cur.memset_bytes, (LONG64)bytes);
}

// Records a GPU timestamp at the current position of the frame's command list
// and remembers which stage it closes.
void av1_perf_gpu_mark(Av1Core *dec, int stage) {
  av1_frame_thread_data *td = dec->curr_frame_data;
  if (!td) return;
  av1_perf_td *p = &td->perf;
  if (!p->gpu_active || p->mark_count >= Av1PerfMaxMarks) return;
  dec->compute.command_list->EndQuery(p->query_heap.Get(), D3D12_QUERY_TYPE_TIMESTAMP, p->mark_count);
  p->mark_sub[p->mark_count] = 0;
  p->mark_stage[p->mark_count++] = (uint8_t)stage;
}

// Inter breakdown: the record of the frame of the current command list, cleared for the stage code to fill,
// or NULL when the frame has none.
av1_perf_inter_detail *av1_perf_inter_begin(Av1Core *dec) {
  av1_frame_thread_data *td = dec->curr_frame_data;
  if (!td) return NULL;
  av1_perf_td *p = &td->perf;
  if (!p->detail_mode) return NULL;
  if (!p->detail) p->detail = (av1_perf_inter_detail *)malloc(sizeof(av1_perf_inter_detail));
  if (!p->detail) {
    p->detail_mode = 0;
    return NULL;
  }
  memset(p->detail, 0, sizeof(*p->detail));
  p->detail->mode = p->detail_mode;
  p->detail_filled = 1;
  return p->detail;
}

// Inter breakdown: a timestamp behind a section of the inter stage, or behind one of its dispatches. Its
// time is counted to |stage| as well, so the stage keeps the time it has without the breakdown.
void av1_perf_inter_mark(Av1Core *dec, int stage, int section, int list) {
  av1_frame_thread_data *td = dec->curr_frame_data;
  if (!td) return;
  av1_perf_td *p = &td->perf;
  if (!p->gpu_active || !p->detail_mode || !p->detail_filled || p->mark_count >= Av1PerfMaxMarks) return;
  dec->compute.command_list->EndQuery(p->query_heap.Get(), D3D12_QUERY_TYPE_TIMESTAMP, p->mark_count);
  p->mark_sub[p->mark_count] = (uint16_t)((section + 1) | ((list + 1) << 8));
  p->mark_stage[p->mark_count++] = (uint8_t)stage;
  ++p->detail->marks;
}

// Start of a frame's command list: decide whether this frame is timed and
// create the slot's query heap and readback buffer on first use.
static void perf_begin_command_buffer(Av1Core *dec, av1_frame_thread_data *td) {
  av1_perf_td *p = &td->perf;
  p->gpu_active = 0;
  p->mark_count = 0;
  p->detail_mode = 0;
  p->detail_filled = 0;
  p->collect = perf_enabled();
  if (!p->collect) return;
  dec->perf_cur.t_record_start = perf_now();
  if (!dec->perf_cur.t_frame_start) dec->perf_cur.t_frame_start = dec->perf_cur.t_record_start;

  if (!p->query_heap && !p->gpu_failed) {
    dx_compute_context *compute = &dec->compute;
    // Timestamp queries need a direct or compute queue; the frequency must
    // come from the queue the decoder's lists execute on.
    const D3D12_COMMAND_LIST_TYPE queue_type = compute->queue->GetDesc().Type;
    UINT64 freq = 0;
    int ok = (queue_type == D3D12_COMMAND_LIST_TYPE_DIRECT || queue_type == D3D12_COMMAND_LIST_TYPE_COMPUTE) &&
             SUCCEEDED(compute->queue->GetTimestampFrequency(&freq)) && freq != 0;
    if (ok) {
      D3D12_QUERY_HEAP_DESC heap_desc = {};
      heap_desc.Type = D3D12_QUERY_HEAP_TYPE_TIMESTAMP;
      heap_desc.Count = Av1PerfMaxMarks;
      ok = SUCCEEDED(compute->device->CreateQueryHeap(&heap_desc, IID_PPV_ARGS(&p->query_heap)));
    }
    if (ok) {
      D3D12_HEAP_PROPERTIES heap_props = {};
      heap_props.Type = D3D12_HEAP_TYPE_READBACK;
      D3D12_RESOURCE_DESC buf_desc = {};
      buf_desc.Dimension = D3D12_RESOURCE_DIMENSION_BUFFER;
      buf_desc.Width = sizeof(UINT64) * Av1PerfMaxMarks;
      buf_desc.Height = 1;
      buf_desc.DepthOrArraySize = 1;
      buf_desc.MipLevels = 1;
      buf_desc.Format = DXGI_FORMAT_UNKNOWN;
      buf_desc.SampleDesc.Count = 1;
      buf_desc.Layout = D3D12_TEXTURE_LAYOUT_ROW_MAJOR;
      ok = SUCCEEDED(compute->device->CreateCommittedResource(&heap_props, D3D12_HEAP_FLAG_NONE, &buf_desc,
                                                              D3D12_RESOURCE_STATE_COPY_DEST, NULL,
                                                              IID_PPV_ARGS(&p->readback)));
    }
    if (ok) {
      dec->perf_gpu_freq = freq;
    } else {
      p->query_heap.Reset();
      p->readback.Reset();
      p->gpu_failed = 1;
    }
  }
  // The inter breakdown of a decoded frame, in the mode its tiles were counted with (av1_setup_frame). Its
  // counters need no timestamps.
  if (!td->output_only) p->detail_mode = dec->perf_inter_mode;
  if (p->query_heap) {
    p->gpu_active = 1;
    av1_perf_gpu_mark(dec, AV1_PERF_GPU_BEGIN);
  }
}

// GPU completion thread, after the frame's fence: build the record from the
// CPU snapshot taken at submit and the resolved GPU timestamps.
static void perf_collect(Av1Core *dec, av1_frame_thread_data *td, int64_t wait_start, av1_perf_frame *rec) {
  av1_perf_td *p = &td->perf;
  const int64_t now = perf_now();
  LARGE_INTEGER qpf;
  QueryPerformanceFrequency(&qpf);
  const double to_ms = 1000.0 / (double)qpf.QuadPart;

  memset(rec, 0, sizeof(*rec));
  rec->frame_number = p->acc.frame_number;
  rec->show_frame = p->acc.show_frame;
  rec->showable_frame = p->acc.showable_frame;
  rec->is_sef = p->acc.is_sef;
  rec->intra_frame = p->acc.intra_frame;
  rec->used_sec_slot = p->acc.used_sec_slot;
  rec->intra_iters = p->acc.intra_iters;
  rec->intra_v2 = p->acc.intra_v2;
  rec->intra_dispatches = p->acc.intra_stats[0];
  rec->intra_barriers = p->acc.intra_stats[1];
  rec->intra_groups = p->acc.intra_stats[2];
  rec->intra_passes = p->acc.intra_stats[3];
  rec->tile_count = p->acc.tile_count;
  for (int i = 0; i < AV1_PERF_CPU_COUNT; ++i) rec->cpu_ms[i] = (double)p->acc.ticks[i] * to_ms;
  rec->cpu_ms[AV1_PERF_CPU_FENCE_WAIT] = (double)(now - wait_start) * to_ms;
  rec->cpu_ms[AV1_PERF_CPU_GPU_LATENCY] = (double)(now - p->t_submit) * to_ms;
  rec->memset_bytes = p->acc.memset_bytes;
  rec->complete_ms = (double)now * to_ms;
  rec->gpu_gap_ms = -1.0;

  // The inter breakdown goes to the callback as a copy this thread owns: the slot goes back to the pool
  // before the callback runs, and the decode thread may be recording the slot's next frame by then.
  av1_perf_inter_detail *detail = NULL;
  if (p->detail_mode && p->detail_filled && p->detail) {
    if (!dec->perf_detail_out)
      dec->perf_detail_out = (av1_perf_inter_detail *)malloc(sizeof(av1_perf_inter_detail));
    detail = dec->perf_detail_out;
    if (detail) memcpy(detail, p->detail, sizeof(*detail));
  }
  rec->inter = detail;

  if (!p->gpu_active || p->mark_count <= 0 || !dec->perf_gpu_freq) return;
  UINT64 ts[Av1PerfMaxMarks];
  D3D12_RANGE read_range = {0, sizeof(UINT64) * (size_t)p->mark_count};
  void *ptr = NULL;
  if (FAILED(p->readback->Map(0, &read_range, &ptr)) || !ptr) return;
  memcpy(ts, ptr, read_range.End);
  D3D12_RANGE written = {0, 0};
  p->readback->Unmap(0, &written);

  const double gpu_to_ms = 1000.0 / (double)dec->perf_gpu_freq;
  for (int i = 1; i < p->mark_count; ++i) {
    const int stage = p->mark_stage[i];
    const int sub = p->mark_sub[i];
    const double ms = ts[i] > ts[i - 1] ? (double)(ts[i] - ts[i - 1]) * gpu_to_ms : 0.0;
    if (stage < AV1_PERF_GPU_STAGE_COUNT) {
      rec->gpu_stage_ms[stage] += ms;
      if (!sub) ++rec->gpu_stage_hits[stage];
    }
    rec->gpu_sum_ms += ms;
    if (detail) {
      // A timestamp of the breakdown, or the one that closes the inter stage (behind its last barrier) or
      // the reconstruction.
      int section = sub ? (sub & 255) - 1
                        : stage == AV1_PERF_GPU_INTER ? (int)AV1_PERF_INTER_SYNC2
                                                      : stage == AV1_PERF_GPU_RECON ? (int)AV1_PERF_INTER_RECON : -1;
      const int list = sub ? (sub >> 8) - 1 : -1;
      if (section >= 0 && section < AV1_PERF_INTER_SECTION_COUNT) {
        detail->sections[section].ms += ms;
        detail->sections[section].timed = 1;
        if (list >= 0 && list < AV1_PERF_INTER_LISTS) detail->sections[section].lists[list].ms += ms;
      }
    }
  }
  const UINT64 first = ts[0];
  const UINT64 last = ts[p->mark_count - 1];
  rec->gpu_span_ms = last > first ? (double)(last - first) * gpu_to_ms : 0.0;
  if (dec->perf_gpu_last_end)
    rec->gpu_gap_ms = first > dec->perf_gpu_last_end ? (double)(first - dec->perf_gpu_last_end) * gpu_to_ms : 0.0;
  dec->perf_gpu_last_end = last;
  rec->gpu_marks = p->mark_count;
  rec->gpu_valid = 1;
}

// Test player switch (-asyncclear): the per-frame clears of a slot's grids move from the decode thread to the
// GPU completion thread. Off unless av1_set_async_clear(1) was called. See av1_setup_frame for the rules.
static int g_async_clear = 0;

extern "C" void av1_set_async_clear(int enable) { g_async_clear = enable; }

// The build with /p:Rav1gUavSync=1 compiles the shaders with GroupMemoryBarrierWithGroupSync defined as
// AllMemoryBarrierWithGroupSync and this file with RAV1G_UAVSYNC.
extern "C" const char *av1_shader_variant(void) {
#ifdef RAV1G_UAVSYNC
  return "uavsync";
#else
  return "";
#endif
}

// GPU completion thread, -asyncclear: the frame that used |td| has completed on the GPU, so nothing reads the
// slot's grids any more, and the slot is not in the pool, so the decode thread cannot have it. Clear what the
// frame cleared at its setup; the next frame of the same size then starts on a slot that is already clean.
// Returns the bytes cleared.
static size_t async_clear_slot(av1_frame_thread_data *td) {
  size_t bytes = 0;
  if (td->clear_pending && td->async_clear) {
    memset(td->mode_info_grid->host_ptr, 0, td->clear_mi_bytes);
    // The luma grid and the chroma grid behind it, as one range (the two memsets of av1_setup_frame).
    if (td->clear_iter_bytes) memset(td->gen_intra_inter_grid->host_ptr, -1, td->clear_iter_bytes);
    td->clean_mi_bytes = td->clear_mi_bytes;
    td->clean_iter_bytes = td->clear_iter_bytes;
    bytes = td->clear_mi_bytes + td->clear_iter_bytes;
  }
  td->clear_pending = 0;
  return bytes;
}

// ---------------------------------------------------------------------------
// Test program flag -dumpstages (av1_set_stage_dump in av1_perf.h): copies of the buffers behind the stages of
// a frame's command list. Inert until a callback and a frame list are set.
// ---------------------------------------------------------------------------
enum { StageDumpFramesMax = 256 };
static av1_stage_dump_cb_fn_t g_stage_dump_cb = NULL;
static void *g_stage_dump_priv = NULL;
static int g_stage_dump_frames[StageDumpFramesMax];
static int g_stage_dump_count = 0;

extern "C" void av1_set_stage_dump(av1_stage_dump_cb_fn_t cb, void *priv, const int *frames, int count) {
  g_stage_dump_cb = NULL;
  g_stage_dump_count = 0;
  if (!cb || !frames || count <= 0) return;
  if (count > StageDumpFramesMax) count = StageDumpFramesMax;
  memcpy(g_stage_dump_frames, frames, sizeof(int) * (size_t)count);
  g_stage_dump_count = count;
  g_stage_dump_priv = priv;
  g_stage_dump_cb = cb;
}

static int stage_dump_wanted(int frame_number) {
  if (!g_stage_dump_cb) return 0;
  for (int i = 0; i < g_stage_dump_count; ++i)
    if (g_stage_dump_frames[i] == frame_number) return 1;
  return 0;
}

// Start of a command list: is its frame dumped. Copies of a frame that was never submitted are dropped here.
static void stage_dump_begin(av1_frame_thread_data *td, int is_sef) {
  for (int i = 0; i < td->dump_count; ++i) {
    td->dumps[i].readback.Reset();
    free(td->dumps[i].layout);
    td->dumps[i].layout = NULL;
  }
  td->dump_count = 0;
  td->dump_is_sef = is_sef;
  td->dump_info[0] = 0;
  td->dump_active = stage_dump_wanted(td->frame_number);
}

// A new dump of the frame of the current command list: a readback buffer of |size| bytes for its copies.
static av1_stage_dump_entry *stage_dump_new(Av1Core *dec, const char *stage, size_t size) {
  av1_frame_thread_data *td = dec->curr_frame_data;
  if (!td || !td->dump_active || !size) return NULL;
  if (td->dump_count >= Av1StageDumpMax) {
    av1_log(1, "DUMPSTAGES frame %d: no room for the dump of stage %s", td->frame_number, stage);
    return NULL;
  }
  av1_stage_dump_entry *e = &td->dumps[td->dump_count];
  D3D12_HEAP_PROPERTIES heap_props = {};
  heap_props.Type = D3D12_HEAP_TYPE_READBACK;
  D3D12_RESOURCE_DESC buf_desc = {};
  buf_desc.Dimension = D3D12_RESOURCE_DIMENSION_BUFFER;
  buf_desc.Width = size;
  buf_desc.Height = 1;
  buf_desc.DepthOrArraySize = 1;
  buf_desc.MipLevels = 1;
  buf_desc.Format = DXGI_FORMAT_UNKNOWN;
  buf_desc.SampleDesc.Count = 1;
  buf_desc.Layout = D3D12_TEXTURE_LAYOUT_ROW_MAJOR;
  const HRESULT hr = dec->compute.device->CreateCommittedResource(
      &heap_props, D3D12_HEAP_FLAG_NONE, &buf_desc, D3D12_RESOURCE_STATE_COPY_DEST, NULL, IID_PPV_ARGS(&e->readback));
  if (FAILED(hr)) {
    av1_log(1, "DUMPSTAGES frame %d: no readback buffer of %llu bytes for stage %s (hr 0x%08X)", td->frame_number,
            (unsigned long long)size, stage, (unsigned)hr);
    e->readback.Reset();
    return NULL;
  }
  memset(e->planes, 0, sizeof(e->planes));
  e->stage = stage;
  e->layout = NULL;
  e->kind = 0;
  e->bytes_per_sample = td->is_hbd ? 2 : 1;
  e->bit_depth = td->bitdepth;
  e->size = size;
  ++td->dump_count;
  return e;
}

// Records the copy of |size| bytes of |src| from |offset| to |dst_offset| of the dump's readback buffer.
// |uav|: |src| is a buffer the shaders write (unordered access state), so it is a copy source for the length of
// the copy. Otherwise it is a buffer in the generic read state (upload or host memory), which is a copy source
// as it is.
static void stage_dump_copy_region(Av1Core *dec, av1_stage_dump_entry *e, size_t dst_offset, ID3D12Resource *src,
                                   size_t offset, size_t size, int uav) {
  ID3D12GraphicsCommandList *command_list = dec->compute.command_list.Get();
  if (uav)
    command_list->ResourceBarrier(1, &CD3DX12_RESOURCE_BARRIER::Transition(src, D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
                                                                           D3D12_RESOURCE_STATE_COPY_SOURCE));
  command_list->CopyBufferRegion(e->readback.Get(), dst_offset, src, offset, size);
  if (uav)
    command_list->ResourceBarrier(1, &CD3DX12_RESOURCE_BARRIER::Transition(src, D3D12_RESOURCE_STATE_COPY_SOURCE,
                                                                           D3D12_RESOURCE_STATE_UNORDERED_ACCESS));
}

// Records the copy of |size| bytes of |src| from |offset| into a readback buffer of its own. |src| is a
// buffer the shaders write (unordered access state).
static av1_stage_dump_entry *stage_dump_copy(Av1Core *dec, const char *stage, ID3D12Resource *src, size_t offset,
                                             size_t size) {
  av1_stage_dump_entry *e = stage_dump_new(dec, stage, size);
  if (e) stage_dump_copy_region(dec, e, 0, src, offset, size, 1);
  return e;
}

int av1_stage_dump_active(Av1Core *dec) {
  const av1_frame_thread_data *td = dec->curr_frame_data;
  return td && td->dump_active;
}

void av1_stage_dump_data(Av1Core *dec, const char *stage, const char *layout, const av1_stage_dump_region *regions,
                         int count) {
  size_t size = 0;
  for (int i = 0; i < count; ++i) size += regions[i].size;
  av1_stage_dump_entry *e = stage_dump_new(dec, stage, size);
  if (!e) return;
  e->kind = 2;
  e->bytes_per_sample = 0;
  const size_t len = strlen(layout);
  e->layout = (char *)malloc(len + 1);
  if (e->layout) memcpy(e->layout, layout, len + 1);
  size_t pos = 0;
  for (int i = 0; i < count; ++i) {
    if (!regions[i].size) continue;
    stage_dump_copy_region(dec, e, pos, regions[i].src, regions[i].offset, regions[i].size, regions[i].uav);
    pos += regions[i].size;
  }
}

av1_stage_dump_region av1_stage_dump_cb_region(Av1Core *dec, const ConstantBufferObject &cbo, size_t size) {
  const GpuBufferObject *cb = dec->curr_frame_data->command_buffer.cb_alloc;
  av1_stage_dump_region region = {cb->dev, (size_t)((const char *)cbo.host_ptr - (const char *)cb->host_ptr), size, 0};
  return region;
}

// The mode info the stages of the frame read: the grid (one pointer per 4x4 unit, rows of mi_stride) and the
// records it points to. Of the pool only the records of the frame's tiles are copied, tile by tile: the
// layout lists, per run of records, the pool index of its first record and the number of records.
void av1_stage_dump_mode_info(Av1Core *dec, AV1_COMMON *cm) {
  av1_frame_thread_data *td = dec->curr_frame_data;
  if (!td || !td->dump_active) return;
  char text[512];
  size_t grid_bytes = sizeof(MB_MODE_INFO *) * (size_t)cm->mi_stride * (size_t)cm->mi_rows;
  if (grid_bytes > td->mode_info_grid->size) grid_bytes = td->mode_info_grid->size;
  snprintf(text, sizeof(text), "buffer=mi_grid entry=%d mi_stride=%d mi_cols=%d mi_rows=%d pool_base=%u record=%d\n",
           (int)sizeof(MB_MODE_INFO *), cm->mi_stride, cm->mi_cols, cm->mi_rows,
           static_cast<unsigned int>(reinterpret_cast<uint64_t>(dec->mode_info_pool->host_ptr)),
           (int)sizeof(MB_MODE_INFO));
  const av1_stage_dump_region grid = {td->mode_info_grid->dev, 0, grid_bytes, 0};
  av1_stage_dump_data(dec, "mi_grid", text, &grid, 1);

  const int tile_count = td->tile_count;
  const int runs_max = tile_count * 2;
  av1_stage_dump_region *regions = (av1_stage_dump_region *)malloc(sizeof(av1_stage_dump_region) * (size_t)runs_max);
  const size_t text_max = 128 + (size_t)runs_max * 48;
  char *layout = (char *)malloc(text_max);
  if (regions && layout) {
    const size_t pool_records = dec->mode_info_pool->size / sizeof(MB_MODE_INFO);
    int runs = 0;
    int len = snprintf(layout, text_max, "buffer=mi_pool record=%d\n", (int)sizeof(MB_MODE_INFO));
    for (int pass = 0; pass < 2; ++pass) {
      // The tiles of the frame's slot, then what they continued with in a second slot's share.
      const av1_tile_data *tiles = pass ? (td->sec_thread_data ? td->sec_thread_data->tile_data : NULL) : td->tile_data;
      for (int t = 0; tiles && t < tile_count; ++t) {
        const int first = tiles[t].mi_offset;
        const int records = tiles[t].mi_count;
        if (records <= 0 || first < 0 || (size_t)first + (size_t)records > pool_records) continue;
        regions[runs].src = dec->mode_info_pool->dev;
        regions[runs].offset = sizeof(MB_MODE_INFO) * (size_t)first;
        regions[runs].size = sizeof(MB_MODE_INFO) * (size_t)records;
        regions[runs].uav = 0;
        ++runs;
        len += snprintf(layout + len, text_max - len, "run first=%d count=%d\n", first, records);
      }
    }
    if (runs) av1_stage_dump_data(dec, "mi_pool", layout, regions, runs);
  }
  free(regions);
  free(layout);
}

void av1_stage_dump_fb(Av1Core *dec, const char *stage, const HwFrameBuffer *fb, const HwFrameBuffer *dims) {
  if (!dims) dims = fb;
  // The whole buffer, borders and all, from its start in the pool.
  av1_stage_dump_entry *e = stage_dump_copy(dec, stage, dec->frame_buffer_pool->dev, fb->base_offset, fb->size);
  if (!e) return;
  for (int p = 0; p < 3; ++p) {
    e->planes[p].offset = (uint32_t)(fb->planes[p].offset - fb->base_offset);
    e->planes[p].stride = (uint32_t)fb->planes[p].stride;
    e->planes[p].width = (uint32_t)(p ? dims->uv_crop_width : dims->y_crop_width);
    e->planes[p].height = (uint32_t)(p ? dims->uv_crop_height : dims->y_crop_height);
  }
}

void av1_stage_dump_residuals(Av1Core *dec, const char *stage) {
  av1_frame_thread_data *td = dec->curr_frame_data;
  if (!td || !td->dump_active) return;
  const HwFrameBuffer *fb = td->frame_buffer;
  // The residual planes of the frame: rows of res_stride bytes from res_offset, 128-aligned in both directions.
  size_t size = (size_t)fb->planes[2].res_offset + (size_t)fb->planes[2].res_stride * (((fb->height >> 1) + 127) & ~127);
  if (size > dec->idct_residuals->size) size = dec->idct_residuals->size;
  av1_stage_dump_entry *e = stage_dump_copy(dec, stage, dec->idct_residuals->dev, 0, size);
  if (!e) return;
  e->kind = 1;
  e->bytes_per_sample = 2;
  for (int p = 0; p < 3; ++p) {
    e->planes[p].offset = (uint32_t)fb->planes[p].res_offset;
    e->planes[p].stride = (uint32_t)fb->planes[p].res_stride;
    e->planes[p].width = (uint32_t)(p ? fb->uv_crop_width : fb->y_crop_width);
    e->planes[p].height = (uint32_t)(p ? fb->uv_crop_height : fb->y_crop_height);
  }
}

void av1_stage_dump_output(Av1Core *dec, const char *stage, const HwOutputImage *img, size_t size) {
  // The output image is put together at the start of back_buffer1, in the layout of the image.
  av1_stage_dump_entry *e =
      stage_dump_copy(dec, stage, dec->frame_buffer_pool->dev, dec->back_buffer1.base_offset, size);
  if (!e) return;
  for (int p = 0; p < (img->monochrome ? 1 : 3); ++p) {
    e->planes[p].offset = (uint32_t)img->planes[p].offset;
    e->planes[p].stride = (uint32_t)img->planes[p].stride;
    e->planes[p].width = (uint32_t)(p ? img->uv_crop_width : img->y_crop_width);
    e->planes[p].height = (uint32_t)(p ? img->uv_crop_height : img->y_crop_height);
  }
}

// One line about the frame for its dumps: what a reader needs to know which stages ran and on what.
static void stage_dump_frame_info(Av1Core *dec, AV1_COMMON *cm) {
  av1_frame_thread_data *td = dec->curr_frame_data;
  if (!td->dump_active) return;
  static const char *const frame_types[] = {"key", "inter", "intra_only", "switch"};
  const int type = cm->current_frame.frame_type;
  // Short names: the line has to fit the 255 characters a dump file keeps of it, references included.
  int len = snprintf(td->dump_info, sizeof(td->dump_info),
                     "type=%s show=%d size=%dx%d upscaled_width=%d denom=%d bits=%d q=%d lf=%d,%d,%d,%d cdef=%d "
                     "superres=%d lr=%d grain=%d tiles=%dx%d scaled=%d refs=",
                     (type >= 0 && type < 4) ? frame_types[type] : "?", cm->show_frame, cm->width, cm->height,
                     cm->superres_upscaled_width, cm->superres_scale_denominator, td->bitdepth, cm->base_qindex,
                     cm->lf.filter_level[0], cm->lf.filter_level[1], cm->lf.filter_level_u, cm->lf.filter_level_v,
                     td->do_cdef, td->do_superres, td->do_loop_rest, td->do_filmgrain, cm->tile_cols, cm->tile_rows,
                     td->scale_enable);
  for (int i = 0; i < 7 && len > 0 && len < (int)sizeof(td->dump_info); ++i) {
    const HwFrameBuffer *ref = frame_is_intra_only(cm) ? NULL : td->refs[i];
    if (ref)
      len += snprintf(td->dump_info + len, sizeof(td->dump_info) - len, "%s%d:%dx%d", i ? "," : "",
                      ref->frame_number, ref->y_crop_width, ref->y_crop_height);  // frame number : size
    else
      len += snprintf(td->dump_info + len, sizeof(td->dump_info) - len, "%s-", i ? "," : "");
  }
}

// GPU completion thread, after the frame's fence: hand the copies over and release them.
static void stage_dump_deliver(av1_frame_thread_data *td) {
  const av1_stage_dump_cb_fn_t cb = g_stage_dump_cb;
  for (int i = 0; i < td->dump_count; ++i) {
    av1_stage_dump_entry *e = &td->dumps[i];
    D3D12_RANGE read_range = {0, e->size};
    void *ptr = NULL;
    if (cb && e->readback && SUCCEEDED(e->readback->Map(0, &read_range, &ptr)) && ptr) {
      av1_stage_dump dump = {};
      dump.frame_number = td->frame_number;
      dump.index = i;
      dump.stage = e->stage;
      dump.kind = e->kind;
      dump.bytes_per_sample = e->bytes_per_sample;
      dump.bit_depth = e->bit_depth;
      dump.is_sef = td->dump_is_sef;
      dump.info = td->dump_info;
      memcpy(dump.planes, e->planes, sizeof(dump.planes));
      dump.data = (const uint8_t *)ptr;
      dump.size = e->size;
      dump.layout = e->layout;
      cb(g_stage_dump_priv, &dump);
      D3D12_RANGE written = {0, 0};
      e->readback->Unmap(0, &written);
    } else if (cb) {
      av1_log(1, "DUMPSTAGES frame %d: the dump of stage %s could not be read back", td->frame_number, e->stage);
    }
    e->readback.Reset();
    free(e->layout);
    e->layout = NULL;
  }
  td->dump_count = 0;
  td->dump_active = 0;
}

static int get_random_number_test(int val) {
  unsigned int bit;
  bit = ((val >> 0) ^ (val >> 1) ^ (val >> 3) ^ (val >> 12)) & 1;
  val = (val >> 1) | (bit << 15);
  return val;
}

#define CHECK_RESULT(DST, FUNC, DO_ASSIGN) \
  {                                        \
    auto x = FUNC;                         \
    if (DO_ASSIGN) {                       \
      if (!x)                              \
        return -1;                         \
      else                                 \
        DST = x;                           \
    }                                      \
  }

struct resource_config {
  int bitdepth;
  int width;
  int height;
  int fb_count;
  int ref_fb_count;
  int gpu_pipeline_depth;
  int enable_superres;
  int intra_v2;    // create the buffers of the -intrav2 path
  int inter_fast;  // room in the block maps for the lists of the -interfast path
  int low_memory;  // AV1_LOW_MEMORY_*
};

int av1_allocate_buffers(Av1Core *dec, av1_memory_manager_base *mem, const resource_config &cfg, int do_assign) {
  const int target_width = (cfg.width + 127) & ~127;
  const int target_height = (cfg.height + 127) & ~127;
  const int block_count_4x4 = target_width * target_height * 3 / (2 * 4 * 4);
  const int mi_cols = target_width >> 2;
  const int mi_rows = target_height >> 2;

  const int max_tile_cols = target_width >> 6;
  const int max_tile_rows = target_height >> 6;
  dec->block_count4x4 = block_count_4x4;
  dec->max_width = target_width;
  dec->max_height = target_height;
  dec->max_bitdepth = cfg.bitdepth;

  CHECK_RESULT(dec->pbi_alloc, (void *)mem->host_allocate(sizeof(AV1Decoder)), do_assign);
  CHECK_RESULT(dec->buf_pool_alloc, (void *)mem->host_allocate(sizeof(BufferPool)), do_assign);

  for (int p = 0; p < 5; ++p)
    CHECK_RESULT(dec->above_context_alloc[p], (void **)mem->host_allocate(sizeof(void *) * max_tile_rows), do_assign);
  for (int row = 0; row < max_tile_rows; ++row)
    for (int p = 0; p < 5; ++p)
      CHECK_RESULT(dec->above_context_alloc[p][row], (void *)mem->host_allocate(sizeof(ENTROPY_CONTEXT) * mi_cols),
                   do_assign);

  const int y_max_rst_units = max_tile_cols * max_tile_rows;
  const int uv_max_rst_units = (max_tile_cols >> 1) * (max_tile_rows >> 1);
  CHECK_RESULT(dec->restoration_info_alloc[0],
               (void *)mem->host_allocate(sizeof(RestorationUnitInfo) * y_max_rst_units), do_assign);
  CHECK_RESULT(dec->restoration_info_alloc[1],
               (void *)mem->host_allocate(sizeof(RestorationUnitInfo) * uv_max_rst_units), do_assign);
  CHECK_RESULT(dec->restoration_info_alloc[2],
               (void *)mem->host_allocate(sizeof(RestorationUnitInfo) * uv_max_rst_units), do_assign);
  const int tmvs_size = ((mi_rows + MAX_MIB_SIZE) >> 1) * (mi_cols >> 1);
  CHECK_RESULT(dec->tplmvs_alloc, (void *)mem->host_allocate(sizeof(TPL_MV_REF) * tmvs_size), do_assign);

  const int residuals_size = target_width * target_height +
                             2 * ((((target_width >> 1) + 127) & (~127)) * (((target_height >> 1) + 127) & (~127)));
  CHECK_RESULT(dec->idct_residuals, mem->create_buffer(sizeof(short) * residuals_size, MemoryType::DeviceOnly, "idct_residuals", -1),
               do_assign);
  CHECK_RESULT(dec->idct_blocks, mem->create_buffer(block_count_4x4 * IdctBlockSize, MemoryType::DeviceOnly, "idct_blocks", -1),
               do_assign);
  CHECK_RESULT(dec->inter_mask_lut, mem->create_buffer(sizeof(wedge_mask_buf) * 2, MemoryType::DeviceOnlyConst, "inter_mask_lut", -1),
               do_assign);
  CHECK_RESULT(dec->inter_warp_filter, mem->create_buffer(sizeof(warped_filter) * 2, MemoryType::DeviceOnlyConst, "inter_warp_filter", -1),
               do_assign);

  CHECK_RESULT(dec->filmgrain_noise, mem->create_buffer(sizeof(int) * (96 * 96 + 48 * 48 * 2), MemoryType::DeviceOnly, "filmgrain_noise", -1),
               do_assign);
  CHECK_RESULT(dec->filmgrain_gaus, mem->create_buffer(sizeof(dx_gaussian_sequence), MemoryType::DeviceOnlyConst, "filmgrain_gaus", -1),
               do_assign);
  CHECK_RESULT(dec->filmgrain_random_luma, mem->create_buffer(sizeof(int) * (65536 + 1), MemoryType::DeviceOnlyConst, "filmgrain_random_luma", -1),
               do_assign);
  CHECK_RESULT(dec->filmgrain_random_chroma, mem->create_buffer(sizeof(int) * (65536 + 1), MemoryType::DeviceOnlyConst, "filmgrain_random_chroma", -1),
               do_assign);

  // frame buffer size:
  const int border = 16;
  const int y_stride = target_width + 2 * border;
  const int y_size = y_stride * target_height;
  const int uv_stride = (target_width >> 1) + border * 2;
  const int uv_size = uv_stride * (target_height >> 1);
  const int bpp = cfg.bitdepth > 8 ? 2 : 1;
  const int fb_size = ((y_size + 2 * uv_size) * bpp + 255) & ~255;

  // The pool buffer starts with the back buffers that are not handed out: back_buffer1, and behind it the
  // back buffer the frames share when they do not take one from the pool.
  dec->shared_back_buffer = (cfg.low_memory & AV1_LOW_MEMORY_SHARED_BACK_BUFFER) != 0;
  dec->low_memory = cfg.low_memory;
  dec->fb_size = fb_size;
  dec->fb_offset = dec->shared_back_buffer ? 2 * fb_size : fb_size;
  dec->enable_superres = cfg.enable_superres;
  CHECK_RESULT(dec->frame_buffer_pool,
               mem->create_buffer((size_t)fb_size * cfg.fb_count + dec->fb_offset, MemoryType::DeviceOnly, "frame_buffer_pool", -1), do_assign);

  const int grid_w = mi_cols + 2 + 128;
  const int grid_h = mi_rows + 2 + 128;
  const int cdef_blocks = target_width * target_height / 64 / 64;

  const int block_count_8x8 = block_count_4x4 >> 2;
  const int mi_size = mi_cols * mi_rows;
  const int lf_blk_count = (target_width / 64) * (target_height / 4) +       // vert luma
                           (target_width / 128) * (target_height / 8) * 2 +  // vert chroma
                           (target_height / 64) * (target_width / 4) +       // hor luma
                           (target_height / 128) * (target_width / 8) * 2;   // hor chroma

  const int max_tiles = max_tile_cols * max_tile_rows;

  dec->pred_map_size = (256 + 32) * max_tiles + (mi_size >> 2) * 10;
  // -interfast: the lists by motion vector class of every tile, behind the block maps as they always were.
  dec->inter_fast_tiles = cfg.inter_fast ? max_tiles : 0;
  CHECK_RESULT(dec->prediction_blocks, mem->create_buffer(block_count_4x4 * 16 * 2, MemoryType::DeviceOnly, "prediction_blocks", -1), do_assign);
  CHECK_RESULT(dec->prediction_blocks_warp, mem->create_buffer(block_count_8x8 * 48, MemoryType::DeviceOnly, "prediction_blocks_warp", -1),
               do_assign);
  CHECK_RESULT(dec->loopfilter_blocks, mem->create_buffer(lf_blk_count * 32, MemoryType::DeviceOnly, "loopfilter_blocks", -1), do_assign);
  // One share of the mode info pool and of the coefficient buffer per slot (mode_info_offset and
  // coef_buffer_offset below). With the default two slots these are the sizes the code always had.
  //
  // A frame has at most one mode info entry per 4x4 unit and IdctCoefCountDenum (16 luma + 2 * 4 chroma)
  // coefficients per 4x4 unit. A share holds half of the entries and 17/24 of the coefficients; a frame that
  // needs more takes a second slot and continues in that slot's share (av1_setup_sec_data,
  // av1_setup_ext_coef_buffer), so two shares cover the worst case. The low memory shares are a quarter and
  // 12/24: two of them no longer cover every frame, and a frame that runs out is a decode error
  // (decode_mbmi_block, av1_read_coeffs_txb).
  dec->mode_info_shift = (cfg.low_memory & AV1_LOW_MEMORY_MODE_INFO) ? 2 : 1;
  dec->coef_count_num = (cfg.low_memory & AV1_LOW_MEMORY_COEFS) ? (int)IdctCoefCountNumLow : (int)IdctCoefCountNum;
  const int mi_share = mi_size >> dec->mode_info_shift;
  const size_t slot_count = (size_t)cfg.gpu_pipeline_depth;
  CHECK_RESULT(dec->mode_info_pool,
               mem->create_buffer(sizeof(MB_MODE_INFO) * (size_t)mi_share * slot_count, MemoryType::HostRW, "mode_info_pool", -1),
               do_assign);
  const int coef_buffer_size = target_width * target_height * 3 * dec->coef_count_num / (IdctCoefCountDenum * 2);
  CHECK_RESULT(dec->idct_coefs,
               mem->create_buffer(sizeof(int) * (size_t)coef_buffer_size * slot_count, MemoryType::UploadUAV, "idct_coefs", -1),
               do_assign);
  for (int i = 0; i < cfg.gpu_pipeline_depth; ++i) {
    av1_frame_thread_data *td = &dec->frame_thread_data[i];
    td->slot_index = i;
    td->mode_info_max = mi_share;
    td->mode_info_offset = i * mi_share;
    td->coef_buffer_offset = i * coef_buffer_size;
    td->coef_buffer_size = coef_buffer_size * sizeof(int);
    CHECK_RESULT(td->command_buffer.cb_alloc, mem->create_buffer(1024 * 1024, MemoryType::DeviceUpload, "cb_alloc", i), do_assign);
    CHECK_RESULT(td->tile_data, (av1_tile_data *)mem->host_allocate(sizeof(av1_tile_data) * max_tiles), do_assign);
    CHECK_RESULT(td->gen_mi_block_indexes, mem->create_buffer(sizeof(int) * block_count_4x4 * 2, MemoryType::HostRW, "gen_mi_block_indexes", i),
                 do_assign);
    CHECK_RESULT(td->gen_intra_inter_grid, mem->create_buffer(grid_w * grid_h * 6, MemoryType::HostRW, "gen_intra_inter_grid", i), do_assign);
    CHECK_RESULT(td->gen_block_map,
                 mem->create_buffer((dec->pred_map_size + dec->inter_fast_tiles * (int)InterFastMapSize) * sizeof(int),
                                    MemoryType::HostRW, "gen_block_map", i),
                 do_assign);
    // Test player flag -intrav2 (intra_v2_sched.h): 8 bytes per 4x4 cell of luma and chroma, then the region
    // list, which cannot have more entries than there are cells; two words per 64x64 region on the host.
    // Only for a decoder that was created for that path (aom_codec_dec_cfg_t::intra_v2): nothing else reads
    // or writes them, and intra_v2_setup_frame() keeps the original shaders when they are not there.
    td->intra_v2_cells = 0;
    if (cfg.intra_v2) {
      td->intra_v2_cells = mi_size + (mi_size >> 2);
      CHECK_RESULT(td->intra_v2_buffer, mem->create_buffer((size_t)td->intra_v2_cells * 16, MemoryType::HostRW, "intra_v2_buffer", i),
                   do_assign);
      CHECK_RESULT(td->intra_v2_regions, (unsigned int *)mem->host_allocate(sizeof(int) * 2 * (mi_size >> 8)),
                   do_assign);
    }
    td->intra_v2_sched.stamp = 0;
    td->intra_v2_cols = 0;
    td->intra_v2_rows = 0;
    td->async_clear = 0;
    td->clear_pending = 0;
    td->clear_mi_bytes = 0;
    td->clear_iter_bytes = 0;
    td->clean_mi_bytes = 0;
    td->clean_iter_bytes = 0;
    CHECK_RESULT(td->mode_info_grid, mem->create_buffer(sizeof(MB_MODE_INFO *) * mi_size, MemoryType::HostRW, "mode_info_grid", i),
                 do_assign);
    CHECK_RESULT(td->idct_blocks_unordered,
                 mem->create_buffer(block_count_4x4 * IdctBlockSize, MemoryType::DeviceUpload, "idct_blocks_unordered", i), do_assign);

    CHECK_RESULT(td->cdef_indexes, mem->create_buffer(sizeof(int) * cdef_blocks, MemoryType::DeviceUpload, "cdef_indexes", i), do_assign);
    CHECK_RESULT(td->cdef_skips, mem->create_buffer(sizeof(int) * cdef_blocks * 16 * 16, MemoryType::DeviceUpload, "cdef_skips", i),
                 do_assign);
    CHECK_RESULT(td->loop_rest_types, mem->create_buffer(16 * (block_count_4x4 >> 8), MemoryType::DeviceUpload, "loop_rest_types", i),
                 do_assign);
    CHECK_RESULT(td->loop_rest_wiener, mem->create_buffer(64 * (block_count_4x4 >> 8), MemoryType::DeviceUpload, "loop_rest_wiener", i),
                 do_assign);
    CHECK_RESULT(td->filmgrain_rand_offset,
                 mem->create_buffer(sizeof(int) * ((target_width / 32 + 4) * (target_height / 32 + 1)),
                                    MemoryType::DeviceUpload, "filmgrain_rand_offset", i),
                 do_assign);

    // No palette buffer here: palette_buffer_for_frame() creates the slot's on the first frame that can have
    // palette blocks.
  }
  // Output contexts (av1_decode_sef): the constants of one output copy and the film grain offsets of a frame.
  for (int i = 0; i < OutputThreadDataCount; ++i) {
    av1_frame_thread_data *td = &dec->output_thread_data[i];
    td->output_only = 1;
    td->slot_index = -1;
    CHECK_RESULT(td->command_buffer.cb_alloc,
                 mem->create_buffer(OutputCbSize, MemoryType::DeviceUpload, "output_cb_alloc", -1), do_assign);
    CHECK_RESULT(td->filmgrain_rand_offset,
                 mem->create_buffer(sizeof(int) * ((target_width / 32 + 4) * (target_height / 32 + 1)),
                                    MemoryType::DeviceUpload, "output_filmgrain_rand_offset", -1),
                 do_assign);
  }
  dec->palette_size = (size_t)fb_size;

  const int mvs_size = (target_width >> 3) * (target_height >> 3) * sizeof(MV_REF);
  const int seg_size = (target_width >> 2) * (target_height >> 2);
  for (int i = 0; i < cfg.ref_fb_count; ++i) {
    HwFrameBuffer *fb = &dec->fb_pool_src[i];
    CHECK_RESULT(fb->mvs_alloc, mem->host_allocate(mvs_size), do_assign);
    CHECK_RESULT(fb->seg_alloc, (uint8_t *)mem->host_allocate(seg_size), do_assign);
  }
  return 0;
}

// Pipeline slots asked for by the decoder config: 0 is the default, anything else is clamped to what the
// arrays are sized for.
static int frame_slots_from_cfg(const aom_codec_dec_cfg_t *cfg) {
  if (cfg->frame_slots == 0) return FrameThreadDataDefault;
  return AOMMAX((int)FrameThreadDataDefault, AOMMIN((int)FrameThreadDataCount, cfg->frame_slots));
}

// The same for the memory query and for the creation: both must count the same allocations.
static void resource_config_from_cfg(resource_config *rcfg, const aom_codec_dec_cfg_t *cfg) {
  rcfg->bitdepth = cfg->bitdepth;
  rcfg->width = cfg->width;
  rcfg->height = cfg->height;
  rcfg->gpu_pipeline_depth = frame_slots_from_cfg(cfg);
  rcfg->intra_v2 = cfg->intra_v2 != 0;
  rcfg->inter_fast = cfg->inter_fast != 0;
  // 0 is the default (the shared back buffer); AV1_LOW_MEMORY_NONE is how an application asks for none.
  rcfg->low_memory = cfg->low_memory ? (cfg->low_memory & AV1_LOW_MEMORY_ALL) : AV1_LOW_MEMORY_DEFAULT;
  // Frame buffers in the pool. A buffer is out of the pool while the entropy decoder or a frame in flight
  // holds it:
  //   - the reference map: at most 8 different buffers;
  //   - the frame being decoded: 1, taken while its header is read (av1_reallocate_frame_buffer);
  //   - its back buffer, when the frame is filtered after its reconstruction (CDEF, loop restoration,
  //     superres): 1, taken in av1_setup_frame();
  //   - a frame that failed after av1_setup_frame(): its buffer and its back buffer stay with its slot until
  //     the next av1_setup_frame(), which comes after the next frame has taken its own buffer: 2;
  //   - every other frame in flight, until its GPU work is done: its back buffer, and what the map has
  //     dropped since the frame started (its own buffer when it is not a reference, a reference it reads that
  //     a later frame replaced): 1 + 1 when frames replace one map entry each.
  // Both takes wait until a frame in flight returns a buffer, so what must never be undercut is the count
  // with nothing in flight: 8 + 2 + 1 = 11 (map, failed frame, the next frame's buffer). Below that the
  // decode thread waits for a buffer nobody returns. The 12 of the code before this is 8 for the map, 1 + 1
  // for the frame being decoded and 1 + 1 for the one frame that is in flight next to it with two slots: no
  // wait in the common case. It is kept, and so is the one buffer more for each slot above two.
  // With the shared back buffer no frame takes a back buffer from the pool: the count with nothing in flight
  // is 8 + 1 + 1 = 10 (map, failed frame, next frame), and the common case with two slots is also
  // 8 + 1 + 1 = 10 (map, frame being decoded, one dropped reference of the frame in flight).
  const int base_fb_count = (rcfg->low_memory & AV1_LOW_MEMORY_SHARED_BACK_BUFFER)
                                ? (int)RefFrameBufferCountSharedBack
                                : (int)RefFrameBufferCount;
  rcfg->ref_fb_count = base_fb_count + rcfg->gpu_pipeline_depth - FrameThreadDataDefault;
  rcfg->fb_count = rcfg->ref_fb_count;
  rcfg->enable_superres = 1;
}

// The device bytes av1_allocate_buffers() creates for |rcfg|, and the bytes of the buffers that are created
// later, when a stream needs them: one palette buffer per slot. |dec| is only written where the creation
// writes the same values.
static void planned_device_bytes(Av1Core *dec, const resource_config &rcfg, size_t *at_creation, size_t *deferred) {
  av1_memory_allocator_dummy plan;
  av1_allocate_buffers(dec, &plan, rcfg, 0);
  *at_creation = plan.get_device_size();
  *deferred = (size_t)rcfg.gpu_pipeline_depth * ((dec->palette_size + 255) & ~(size_t)255);
}

extern "C" int av1_query_memory_requirements(aom_codec_dec_cfg_t *cfg) {
  av1_memory_allocator_dummy mem;

  mem.host_allocate(32 * 1024);  // assume aom_codec_alg_priv size, actually ~20kb;
  mem.host_allocate(sizeof(av1_memory_allocator));
  mem.host_allocate(sizeof(Av1Core));
  resource_config rcfg;
  resource_config_from_cfg(&rcfg, cfg);
  Av1Core dec;
  av1_allocate_buffers(&dec, &mem, rcfg, 0);
  cfg->host_size = mem.get_host_size();
  cfg->device_size = mem.get_device_size();
  cfg->device_size_deferred = (size_t)rcfg.gpu_pipeline_depth * ((dec.palette_size + 255) & ~(size_t)255);
  return 0;
}

int create_device(dx_compute_context *context) {
  HRESULT hr = S_OK;
  if (context->device == 0) {
    UINT dxgiFactoryFlags = 0;
#if defined(_DEBUG)
    {
      ComPtr<ID3D12Debug> debugController;
      hr = D3D12GetDebugInterface(IID_PPV_ARGS(&debugController));
      if (SUCCEEDED(hr)) {
        debugController->EnableDebugLayer();
        dxgiFactoryFlags |= DXGI_CREATE_FACTORY_DEBUG;
      }
    }
#endif

    ComPtr<IDXGIFactory4> factory;
    hr = CreateDXGIFactory2(dxgiFactoryFlags, IID_PPV_ARGS(&factory));
    if (FAILED(hr)) return hr;

    ComPtr<IDXGIAdapter1> hardwareAdapter;
    hr = E_FAIL;
    for (int index = 0; DXGI_ERROR_NOT_FOUND != factory->EnumAdapters1(index, &hardwareAdapter); ++index) {
      DXGI_ADAPTER_DESC1 desc;
      hardwareAdapter->GetDesc1(&desc);
      if (desc.Flags & DXGI_ADAPTER_FLAG_SOFTWARE) continue;
      hr = D3D12CreateDevice(hardwareAdapter.Get(), D3D_FEATURE_LEVEL_11_0, IID_PPV_ARGS(&context->device));
      if (SUCCEEDED(hr)) break;
    }

    if (context->device == NULL || FAILED(hr)) return E_FAIL;
  }

  Microsoft::WRL::ComPtr<ID3D12Device> device = context->device;
  D3D12_COMMAND_QUEUE_DESC desc = {};
  if (!context->queue) {
    desc.Flags = D3D12_COMMAND_QUEUE_FLAG_NONE;
    desc.Type = D3D12_COMMAND_LIST_TYPE_DIRECT;
    hr = device->CreateCommandQueue(&desc, IID_PPV_ARGS(&context->queue));
    if (FAILED(hr)) return hr;
  }
  context->queue_direct = context->queue;

  if (FAILED(hr)) return hr;
  return hr;
}

static double to_mib(unsigned long long bytes) { return (double)bytes / (1024.0 * 1024.0); }

// "MEMORY" lines about what the decoder holds: the buffers by memory type, the largest ones, the host pool,
// and what one pipeline slot takes. After a creation, and at the point where a creation failed.
static void log_memory_summary(int is_error, const char *label, const Av1Core *dec, const resource_config &rcfg) {
  const av1_memory_allocator *mem = dec->memory;
  const unsigned long long total = mem->device_bytes();
  av1_log(is_error,
          "MEMORY %s: %ux%u %u-bit, slots %d, frame buffers %d + %d back buffer%s of %d bytes (%.1f MiB) each; "
          "%d device buffers, %llu bytes (%.1f MiB); host pool %llu of %llu bytes (%.1f MiB) in use",
          label, (unsigned)rcfg.width, (unsigned)rcfg.height, (unsigned)rcfg.bitdepth, rcfg.gpu_pipeline_depth,
          rcfg.ref_fb_count, dec->shared_back_buffer ? 2 : 1, dec->shared_back_buffer ? "s" : "", dec->fb_size,
          to_mib((unsigned long long)dec->fb_size), mem->buffer_count(), total, to_mib(total),
          (unsigned long long)mem->host_bytes_used(), (unsigned long long)mem->host_bytes_total(),
          to_mib(mem->host_bytes_used()));
  // What was left out or made smaller, and what is still to come: a slot's palette buffer is created on the
  // first frame of the slot that allows screen content tools.
  int palette_buffers = 0;
  for (int i = 0; i < rcfg.gpu_pipeline_depth; ++i) palette_buffers += dec->frame_thread_data[i].palette_buffer != NULL;
  av1_log(is_error,
          "MEMORY %s options: intra_v2 buffers %s; low memory %d (shared back buffer %s, mode info share 1/%d of "
          "the worst case, coefficient share %d/%d); palette buffers %d of %d created, %.1f MiB each, a slot's on "
          "its first frame with screen content tools",
          label, rcfg.intra_v2 ? "yes" : "no (the decoder was not created for -intrav2)", rcfg.low_memory,
          dec->shared_back_buffer ? "yes" : "no", 1 << dec->mode_info_shift, dec->coef_count_num,
          (int)IdctCoefCountDenum, palette_buffers, rcfg.gpu_pipeline_depth,
          to_mib((unsigned long long)dec->palette_size));

  char line[640];
  int len = 0;
  for (int t = 0; t < MemoryTypeCount && len < (int)sizeof(line); ++t) {
    const MemoryType type = (MemoryType)t;
    len += snprintf(line + len, sizeof(line) - len, "%s%s %d buffers %.1f MiB", t ? "; " : "",
                    av1_memory_type_name(type), mem->device_buffers(type), to_mib(mem->device_bytes(type)));
  }
  // The decoder creates buffers only. The output images are textures of the application.
  av1_log(is_error, "MEMORY %s by type: %s; textures 0", label, line);

  // One slot: its own buffers and its share of the two buffers that are divided between the slots.
  unsigned long long slot_own = 0;
  int slot_buffers = 0;
  unsigned long long pool_share = 0;
  unsigned long long coef_share = 0;
  for (int i = 0; i < mem->buffer_count(); ++i) {
    const GpuBufferObject *obj = mem->buffer(i);
    if (obj->slot == 0) {
      slot_own += obj->size;
      ++slot_buffers;
    }
    if (obj == dec->mode_info_pool) pool_share = obj->size / rcfg.gpu_pipeline_depth;
    if (obj == dec->idct_coefs) coef_share = obj->size / rcfg.gpu_pipeline_depth;
  }
  const unsigned long long slot_share = slot_own + pool_share + coef_share;
  av1_log(is_error,
          "MEMORY %s per slot: %llu bytes (%.1f MiB) = %d own buffers %.1f MiB + mode_info_pool share %.1f MiB + "
          "idct_coefs share %.1f MiB; a slot above %d also adds one frame buffer, %.1f MiB",
          label, slot_share, to_mib(slot_share), slot_buffers, to_mib(slot_own), to_mib(pool_share),
          to_mib(coef_share), (int)FrameThreadDataDefault, to_mib((unsigned long long)dec->fb_size));

  // The largest buffers, one entry per name: the buffers of the slots are the same in every slot.
  enum { Largest = 10 };
  const GpuBufferObject *top[Largest] = {};
  int top_count = 0;
  for (int i = 0; i < mem->buffer_count(); ++i) {
    const GpuBufferObject *obj = mem->buffer(i);
    if (obj->slot > 0) continue;
    int pos = top_count;
    while (pos > 0 && top[pos - 1]->size < obj->size) --pos;
    if (pos >= Largest) continue;
    if (top_count < Largest) ++top_count;
    for (int k = top_count - 1; k > pos; --k) top[k] = top[k - 1];
    top[pos] = obj;
  }
  len = 0;
  line[0] = 0;
  for (int i = 0; i < top_count && len < (int)sizeof(line); ++i) {
    const GpuBufferObject *obj = top[i];
    len += snprintf(line + len, sizeof(line) - len, "%s%s %.1f MiB %s%s", i ? "; " : "", obj->name,
                    to_mib(obj->size), av1_memory_type_name(obj->memtype), obj->slot == 0 ? " (per slot)" : "");
  }
  av1_log(is_error, "MEMORY %s largest: %s", label, line);
}

static void destroy_gpu_decoder(Av1Core *dec, int log_steps);

// A step of the decoder creation failed. Says which one, what was allocated up to there and what the video
// memory budget is, then releases everything the creation has built, so that nothing of it is left when the
// application tries again: the caller never gets a decoder to destroy.
static int create_failed(Av1Core *dec, const resource_config &rcfg, const av1_video_memory *before,
                         const char *step, HRESULT hr) {
  ID3D12Device *device = dec->compute.device.Get();
  // An allocation of the decoder's allocator has logged itself already and is the reason kept for the
  // application; this line is for every other step.
  if (FAILED(hr))
    av1_log_create_error("MEMORY FAILED decoder creation at: %s (hr 0x%08X)", step, (unsigned)hr);
  else
    av1_log_create_error("MEMORY FAILED decoder creation at: %s", step);
  if (dec->memory) log_memory_summary(1, "FAILED decoder, allocated up to the failure", dec, rcfg);
  if (device) {
    av1_video_memory now;
    av1_query_video_memory(device, &now);
    if (before) av1_log_video_memory(1, "FAILED, before the creation", before);
    av1_log_video_memory(1, "FAILED, at the failure", &now);
    // Keep the device for the query after the release.
    device->AddRef();
  }
  destroy_gpu_decoder(dec, 0);
  if (device) {
    av1_video_memory released;
    av1_query_video_memory(device, &released);
    av1_log_video_memory(1, "FAILED, after releasing the failed decoder", &released);
    device->Release();
  }
  return -1;
}

extern "C" int av1_create_gpu_decoder(Av1Core **gpu_dec, aom_codec_dec_cfg_t *cfg) {
  *gpu_dec = NULL;
  av1_clear_create_error();
  if (cfg->host_size < sizeof(av1_memory_allocator)) {
    av1_log_create_error("MEMORY FAILED host pool: %llu bytes given, the allocator alone takes %llu",
                         (unsigned long long)cfg->host_size, (unsigned long long)sizeof(av1_memory_allocator));
    return -1;
  }

  av1_memory_allocator *mem = new (cfg->host_memory) av1_memory_allocator;
  mem->setup((uint8_t *)cfg->host_memory, cfg->host_size);
  mem->host_allocate(sizeof(*mem));
  Av1Core *dec = (Av1Core *)mem->host_allocate(sizeof(Av1Core));
  if (!dec) return -1;  // logged by the allocator; nothing was created yet

  memset(dec, 0, sizeof(*dec));
  dec->memory = mem;

  resource_config rcfg;
  resource_config_from_cfg(&rcfg, cfg);
  dec->frame_slots = rcfg.gpu_pipeline_depth;

  // Everything that cannot fail comes first, so that destroy_gpu_decoder() is valid from here on, whichever
  // step below fails.
  MTQueueInit(&dec->frame_data_pool);
  MTQueueInit(&dec->output_data_pool);
  MTQueueInit(&dec->gpu_item_pool);
  MTQueueInit(&dec->gpu_waiting_queue);
  MTQueueInit(&dec->output_queue);
  MTQueueInit(&dec->image_pool);
  QueueInit(&dec->fb_pool);
  pthread_cond_init(&dec->fb_pool_empty_cond, NULL);
  pthread_mutex_init(&dec->fb_pool_mutex, NULL);
  for (int i = 0; i < dec->frame_slots; ++i) pthread_mutex_init(&dec->frame_thread_data[i].sec_data_mutex, NULL);

  dec->compute.device = static_cast<ID3D12Device *>(cfg->dx12device);
  dec->compute.queue = static_cast<ID3D12CommandQueue *>(cfg->dx12command_queue);
  HRESULT hr = create_device(&dec->compute);
  if (FAILED(hr)) return create_failed(dec, rcfg, NULL, "D3D12 device or command queue", hr);
  dx_compute_context *compute = &dec->compute;
  mem->set_dx_context(compute);

  dec->cb_get_output_image = cfg->out_buffers_cb.get_out_buffer_cb;
  dec->cb_release_image = cfg->out_buffers_cb.release_out_buffer_cb;
  dec->cb_notify_frame_ready = cfg->out_buffers_cb.notify_frame_ready_cb;
  dec->image_alloc_priv = cfg->out_buffers_cb.out_buffers_priv;

  // What the adapter reports before the decoder takes anything.
  av1_video_memory vm_before;
  av1_query_video_memory(compute->device.Get(), &vm_before);

  dec->shader_lib = static_cast<compute_shader_lib *>(cfg->dxPsos);
  if (!dec->shader_lib) return create_failed(dec, rcfg, &vm_before, "no pipeline cache (dxPsos) given", E_POINTER);
  const int shader_errors = wait_shader_create_complete(dec->shader_lib);
  if (shader_errors)
    return create_failed(dec, rcfg, &vm_before, "compute pipeline states (shaders)", (HRESULT)shader_errors);

  // Budget guard: an application over its memory limit loses the D3D12 device for the rest of the process,
  // so with a budget callback the creation is refused before it allocates anything it cannot afford.
  dec->memory_available_cb = cfg->memory_available_cb;
  dec->memory_available_priv = cfg->memory_available_priv;
  {
    size_t planned = 0;
    size_t deferred = 0;
    planned_device_bytes(dec, rcfg, &planned, &deferred);
    int64_t available = dec->memory_available_cb ? dec->memory_available_cb(dec->memory_available_priv) : -1;
    const unsigned long long needed = (unsigned long long)planned + AV1_MEMORY_GUARD_MARGIN;
    if (available >= 0 && needed > (unsigned long long)available) {
      // Not refused at once. The system gives the memory of released D3D12 buffers back some time after the
      // release (a decoder that was destroyed just before this creation), so the answer is asked again for
      // as long as it keeps growing. Bounded: AV1_MEMORY_GUARD_WAIT_MS, and AV1_MEMORY_GUARD_SETTLE_MS
      // without a gain of 1 MiB ends it.
      enum { PollMs = 20 };
      const int64_t first = available;
      int64_t best = available;
      int waited_ms = 0;
      int settle_ms = 0;
      while (needed > (unsigned long long)available && waited_ms < AV1_MEMORY_GUARD_WAIT_MS &&
             settle_ms < AV1_MEMORY_GUARD_SETTLE_MS) {
        Sleep(PollMs);
        waited_ms += PollMs;
        available = dec->memory_available_cb(dec->memory_available_priv);
        if (available < 0) break;
        if (available >= best + (1 << 20)) {
          best = available;
          settle_ms = 0;
        } else {
          settle_ms += PollMs;
        }
      }
      if (available >= 0)
        av1_log(0,
                "MEMORY budget: %.1f MiB needed and %.1f MiB left for the decoder, waited %d ms for memory the "
                "system may still be returning: %.1f MiB left now (%s)",
                to_mib(needed), to_mib((unsigned long long)first), waited_ms,
                to_mib((unsigned long long)available),
                needed <= (unsigned long long)available
                    ? "enough"
                    : (waited_ms >= AV1_MEMORY_GUARD_WAIT_MS ? "still growing at the end of the wait"
                                                             : "no longer growing"));
    }
    if (available >= 0 && needed > (unsigned long long)available) {
      av1_log_create_error(
          "MEMORY REFUSED decoder %ux%u %u-bit, slots %d: its device buffers take %llu bytes (%.1f MiB) and "
          "the creation must leave a margin of %.1f MiB, %.1f MiB together; the application has %.1f MiB left "
          "for the decoder. Nothing was allocated; the D3D12 device is intact",
          (unsigned)rcfg.width, (unsigned)rcfg.height, (unsigned)rcfg.bitdepth, rcfg.gpu_pipeline_depth,
          (unsigned long long)planned, to_mib(planned), to_mib(AV1_MEMORY_GUARD_MARGIN), to_mib(needed),
          to_mib((unsigned long long)available));
      av1_log_video_memory(1, "REFUSED, nothing allocated", &vm_before);
      destroy_gpu_decoder(dec, 0);
      return -1;
    }
    if (available >= 0)
      av1_log(0,
              "MEMORY budget: device buffers %.1f MiB + margin %.1f MiB = %.1f MiB, the application has %.1f "
              "MiB left for the decoder: accepted; palette buffers on first use would add up to %.1f MiB",
              to_mib(planned), to_mib(AV1_MEMORY_GUARD_MARGIN), to_mib(needed),
              to_mib((unsigned long long)available), to_mib(deferred));
    else
      av1_log(0,
              "MEMORY budget: not checked (%s); device buffers %.1f MiB, palette buffers on first use would add "
              "up to %.1f MiB",
              dec->memory_available_cb ? "the application's callback does not know what is left"
                                       : "no memory_available_cb in the decoder configuration",
              to_mib(planned), to_mib(deferred));
  }

  if (av1_allocate_buffers(dec, mem, rcfg, 1))
    return create_failed(dec, rcfg, &vm_before, "decoder buffers (the MEMORY FAILED line above names the one)", S_OK);
  memset(dec->idct_coefs->host_ptr, 0, dec->idct_coefs->size);

  Microsoft::WRL::ComPtr<ID3D12Device> device = compute->device;
  dec->tryhdr10x3 = cfg->tryHDR10x3;

  for (int i = 0; i < dec->frame_slots; ++i) {
    av1_frame_thread_data *td = &dec->frame_thread_data[i];
    ComputeCommandBuffer *cb = &td->command_buffer;
    hr = device->CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT, IID_PPV_ARGS(&cb->allocator));
    if (FAILED(hr)) return create_failed(dec, rcfg, &vm_before, "command allocator of a slot", hr);
    cb->fence_value = 0;
    hr = device->CreateFence(cb->fence_value, D3D12_FENCE_FLAG_NONE, IID_PPV_ARGS(&cb->fence));
    if (FAILED(hr)) return create_failed(dec, rcfg, &vm_before, "fence of a slot", hr);
    cb->event = CreateEvent(nullptr, false, false, nullptr);
    if (!cb->event)
      return create_failed(dec, rcfg, &vm_before, "event of a slot", HRESULT_FROM_WIN32(GetLastError()));
    td->frame_number = 0;
    MTQueuePush(&dec->frame_data_pool, td);
  }
  for (int i = 0; i < OutputThreadDataCount; ++i) {
    av1_frame_thread_data *td = &dec->output_thread_data[i];
    ComputeCommandBuffer *cb = &td->command_buffer;
    hr = device->CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT, IID_PPV_ARGS(&cb->allocator));
    if (FAILED(hr)) return create_failed(dec, rcfg, &vm_before, "command allocator of an output context", hr);
    cb->fence_value = 0;
    hr = device->CreateFence(cb->fence_value, D3D12_FENCE_FLAG_NONE, IID_PPV_ARGS(&cb->fence));
    if (FAILED(hr)) return create_failed(dec, rcfg, &vm_before, "fence of an output context", hr);
    cb->event = CreateEvent(nullptr, false, false, nullptr);
    if (!cb->event)
      return create_failed(dec, rcfg, &vm_before, "event of an output context", HRESULT_FROM_WIN32(GetLastError()));
    td->frame_number = 0;
    MTQueuePush(&dec->output_data_pool, td);
  }

  hr = device->CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_DIRECT,
                                 dec->frame_thread_data[0].command_buffer.allocator.Get(), NULL,
                                 IID_PPV_ARGS(&compute->command_list));
  if (FAILED(hr)) return create_failed(dec, rcfg, &vm_before, "command list", hr);
  hr = compute->command_list->Close();
  if (FAILED(hr)) return create_failed(dec, rcfg, &vm_before, "command list Close", hr);

  hr = av1_upload_luts(dec);
  if (FAILED(hr)) return create_failed(dec, rcfg, &vm_before, "upload of the lookup tables", hr);

  // Work items: one per frame in flight (at most FrameThreadDataCount, each holds a slot) and one per
  // show_existing_frame copy in flight (at most OutputThreadDataCount, each holds an output context). The
  // decode thread waits when the pool is empty.
  for (int i = 0; i < 8; ++i) {
    dec->gpu_item_pool_src[i].data = NULL;
    dec->gpu_item_pool_src[i].image = NULL;
    MTQueuePush(&dec->gpu_item_pool, &dec->gpu_item_pool_src[i]);
  }

  for (int i = 0; i < ImagePoolSize; ++i) {
    HwOutputImage *img = &dec->image_pool_src[i];
    img->size = 0;
    img->fb_ptr = NULL;
    img->is_valid = 0;
    MTQueuePush(&dec->image_pool, img);
  }

  dec->back_buffer1.size = dec->fb_size;
  dec->back_buffer1.base_offset = 0;
  // Only used with shared_back_buffer; the pool buffer has the room for it only then.
  dec->back_buffer0.size = dec->fb_size;
  dec->back_buffer0.base_offset = dec->fb_size;
  for (int i = 0; i < rcfg.ref_fb_count; ++i) {
    const int offset = dec->fb_offset + dec->fb_size * i;
    HwFrameBuffer *fb = &dec->fb_pool_src[i];
    fb->size = dec->fb_size;
    fb->base_offset = offset;
    fb->ref_cnt = 0;
    QueuePush(&dec->fb_pool, fb);
  }

  log_memory_summary(0, "decoder created", dec, rcfg);
  av1_video_memory vm_after;
  av1_query_video_memory(compute->device.Get(), &vm_after);
  av1_log_video_memory(0, "before the decoder's allocations", &vm_before);
  av1_log_video_memory(0, "after the decoder's allocations", &vm_after);

  *gpu_dec = dec;
  return 0;
}

extern "C" void av1_allocate_pbi(Av1Core *dec, AV1Decoder **ppbi, BufferPool **pbp) {
  AV1Decoder *pbi = (AV1Decoder *)dec->pbi_alloc;
  BufferPool *bp = (BufferPool *)dec->buf_pool_alloc;
  memset(pbi, 0, sizeof(*pbi));
  memset(bp, 0, sizeof(*bp));
  pbi->gpu_decoder = dec;
  *ppbi = pbi;
  *pbp = bp;
}

HwFrameBuffer *get_frame_buffer(Av1Core *dec) {
  HwFrameBuffer *fb = NULL;

  pthread_mutex_lock(&dec->fb_pool_mutex);
  {
    while (!dec->fb_pool.m_QueueNotEmpty) pthread_cond_wait(&dec->fb_pool_empty_cond, &dec->fb_pool_mutex);
    fb = (HwFrameBuffer *)QueueGet(&dec->fb_pool);
  }
  fb->ref_cnt = 1;
  pthread_mutex_unlock(&dec->fb_pool_mutex);
  return fb;
}

void frame_buffer_acquire(Av1Core *dec, HwFrameBuffer *fb) {
  pthread_mutex_lock(&dec->fb_pool_mutex);
  ++fb->ref_cnt;
  pthread_mutex_unlock(&dec->fb_pool_mutex);
}

void frame_buffer_release(Av1Core *dec, HwFrameBuffer *fb) {
  pthread_mutex_lock(&dec->fb_pool_mutex);
  --fb->ref_cnt;
  if (fb->ref_cnt == 0) {
    QueuePush(&dec->fb_pool, fb);
    pthread_cond_signal(&dec->fb_pool_empty_cond);
  }
  pthread_mutex_unlock(&dec->fb_pool_mutex);
}

// One "MEMORY destroy <step>: ..." line: what the adapter reports and what the application has left (its
// budget callback) at a step of the release of a decoder. The differences between the lines say which step
// gave how much back, to the video memory and to the application.
static void log_destroy_step(ID3D12Device *device, int64_t (*available_cb)(void *), void *available_priv,
                             const char *step) {
  char dxgi[96];
  char app[96];
  av1_video_memory vm;
  av1_query_video_memory(device, &vm);
  if (SUCCEEDED(vm.hr_adapter) && SUCCEEDED(vm.hr_local))
    snprintf(dxgi, sizeof(dxgi), "dxgi local usage %.1f MiB", to_mib(vm.local.CurrentUsage));
  else
    snprintf(dxgi, sizeof(dxgi), "dxgi local usage not available");
  const int64_t left = available_cb ? available_cb(available_priv) : -1;
  if (left >= 0)
    snprintf(app, sizeof(app), "the application has %.1f MiB left for a decoder", to_mib((unsigned long long)left));
  else
    snprintf(app, sizeof(app), "what the application has left is not known (%s)",
             available_cb ? "its callback does not know" : "no memory_available_cb");
  av1_log(0, "MEMORY destroy %s: %s; %s", step, dxgi, app);
}

// Also for a decoder whose creation stopped half way (create_failed): everything not built yet is zero.
// |log_steps|: the release of a decoder that was in use, logged step by step ("MEMORY destroy").
static void destroy_gpu_decoder(Av1Core *dec, int log_steps) {
  // The device stays with the application; it is held here for the lines after the decoder's own reference
  // is gone. The budget callback is the application's.
  ID3D12Device *device = log_steps ? dec->compute.device.Get() : NULL;
  int64_t (*const available_cb)(void *) = dec->memory_available_cb;
  void *const available_priv = dec->memory_available_priv;
  if (device) {
    device->AddRef();
    log_destroy_step(device, available_cb, available_priv, "start");
  }

  // Joins the GPU completion thread: every frame that was submitted has completed on the GPU (the thread
  // waits for the fence of each), so nothing on the queue uses the buffers that are released below.
  av1_drain_gpu_decoder(dec);

  if (dec->cb_release_image) {
    for (int i = 0; i < ImagePoolSize; ++i) {
      HwOutputImage *img = &dec->image_pool_src[i];
      if (img->is_valid) {
        dec->cb_release_image(dec->image_alloc_priv, img->alloc_priv);
      }
    }
  }

  for (int i = 0; i < dec->frame_slots; ++i) {
    av1_frame_thread_data *td = &dec->frame_thread_data[i];
    if (td->command_buffer.event) CloseHandle(td->command_buffer.event);
    td->command_buffer.event = NULL;
    pthread_mutex_destroy(&td->sec_data_mutex);
  }
  for (int i = 0; i < OutputThreadDataCount; ++i) {
    av1_frame_thread_data *td = &dec->output_thread_data[i];
    if (td->command_buffer.event) CloseHandle(td->command_buffer.event);
    td->command_buffer.event = NULL;
  }
  // The inter breakdown of the timing run (-gputimedetail), where a frame was recorded with it.
  for (int i = 0; i < dec->frame_slots; ++i) {
    free(dec->frame_thread_data[i].perf.detail);
    dec->frame_thread_data[i].perf.detail = NULL;
  }
  free(dec->perf_inter_tiles);
  dec->perf_inter_tiles = NULL;
  dec->perf_inter_tiles_cap = 0;
  free(dec->perf_detail_out);
  dec->perf_detail_out = NULL;

  pthread_cond_destroy(&dec->fb_pool_empty_cond);
  pthread_mutex_destroy(&dec->fb_pool_mutex);
  MTQueueDestroy(&dec->output_queue);
  MTQueueDestroy(&dec->image_pool);
  MTQueueDestroy(&dec->gpu_waiting_queue);
  MTQueueDestroy(&dec->gpu_item_pool);
  MTQueueDestroy(&dec->output_data_pool);
  MTQueueDestroy(&dec->frame_data_pool);

  if (dec->memory) {
    if (device) {
      log_destroy_step(device, available_cb, available_priv, "GPU thread joined, buffers not yet released");
      // The buffers the CPU maps first, the default-heap buffers the CPU never sees last: one line after
      // each memory type.
      static const MemoryType order[MemoryTypeCount] = {DeviceUpload, HostRW,          UploadUAV,
                                                        ReadBack,     DeviceOnlyConst, DeviceOnly};
      for (int t = 0; t < MemoryTypeCount; ++t) {
        int count = 0;
        const size_t bytes = dec->memory->release_type(order[t], &count);
        if (!count) continue;
        char step[96];
        snprintf(step, sizeof(step), "released %s, %d buffers, %.1f MiB", av1_memory_type_name(order[t]), count,
                 to_mib(bytes));
        log_destroy_step(device, available_cb, available_priv, step);
      }
    }
    dec->memory->release();
  }

  // The command list, the allocators and fences of the slots, the timing query heaps, the decoder's command
  // queue (its own unless the application gave one) and its reference to the device.
  dec->~Av1Core();

  if (device) {
    log_destroy_step(device, available_cb, available_priv, "end, command queue, list and allocators released");
    device->Release();
  }
}

void av1_destroy_gpu_decoder(Av1Core *dec) {
  if (!dec) return;
  destroy_gpu_decoder(dec, 1);
}

void av1_prepare_command_buffer(Av1Core *dec) {
  av1_frame_thread_data *td = dec->curr_frame_data;
  dx_compute_context *compute = &dec->compute;
  stage_dump_begin(td, td->output_only);

  td->command_buffer.allocator->Reset();
  compute->command_list->Reset(td->command_buffer.allocator.Get(), NULL);
  td->command_buffer.Reset();

  PutPerfMarker(td, &td->perf_markers[0]);
  perf_begin_command_buffer(dec, td);
}

void av1_commit_command_buffer(Av1Core *dec) {
  av1_frame_thread_data *td = dec->curr_frame_data;
  PutPerfMarker(td, &td->perf_markers[15]);

  dx_compute_context *compute = &dec->compute;
  ComputeCommandBuffer *cb = &td->command_buffer;

  av1_perf_td *perf = &td->perf;
  int64_t perf_submit_start = 0;
  if (perf->collect) {
    // Resolve this frame's timestamps as the last command of its list; they
    // are read back only after the frame's fence has completed.
    if (perf->gpu_active && perf->mark_count > 0)
      compute->command_list->ResolveQueryData(perf->query_heap.Get(), D3D12_QUERY_TYPE_TIMESTAMP, 0,
                                              perf->mark_count, perf->readback.Get(), 0);
    perf_submit_start = perf_now();
  }

  compute->command_list->Close();
  ID3D12CommandList *list[] = {compute->command_list.Get()};

  ++cb->fence_value;
  compute->queue->ExecuteCommandLists(1, list);
  compute->queue->Signal(cb->fence.Get(), cb->fence_value);
  cb->fence->SetEventOnCompletion(cb->fence_value, cb->event);

  if (perf->collect) {
    // Snapshot the CPU side of this frame. The GPU thread cannot see this slot
    // until the caller queues its work item, so plain writes are safe here.
    av1_perf_accum *acc = &dec->perf_cur;
    const int64_t now = perf_now();
    const int64_t waits = acc->ticks[AV1_PERF_CPU_WAIT_OUTPUT] + acc->ticks[AV1_PERF_CPU_WAIT_IMAGE];
    const int64_t record = perf_submit_start - acc->t_record_start - waits;
    acc->ticks[AV1_PERF_CPU_RECORD] = record > 0 ? record : 0;
    acc->ticks[AV1_PERF_CPU_SUBMIT] = now - perf_submit_start;
    acc->ticks[AV1_PERF_CPU_FRAME] = now - acc->t_frame_start;
    memcpy(&perf->acc, acc, sizeof(*acc));
    perf->t_submit = now;
    memset(acc, 0, sizeof(*acc));
  }
}

void PutPerfMarker(av1_frame_thread_data *td, volatile int64_t *marker) {}

extern "C" void av1_setup_context_buffers(AV1Decoder *pbi) {
  AV1_COMMON *cm = &pbi->common;
  Av1Core *dec = pbi->gpu_decoder;
  // above context:
  const int cols = (cm->mi_cols + 31) & ~31;
  const int rows = cm->tile_rows;
  cm->num_allocated_above_contexts = rows;
  cm->num_allocated_above_context_mi_col = cols;
  cm->num_allocated_above_context_planes = cm->seq_params.monochrome ? 1 : MAX_MB_PLANE;
  cm->above_context[0] = (ENTROPY_CONTEXT **)dec->above_context_alloc[0];
  cm->above_context[1] = (ENTROPY_CONTEXT **)dec->above_context_alloc[1];
  cm->above_context[2] = (ENTROPY_CONTEXT **)dec->above_context_alloc[2];
  cm->above_seg_context = (PARTITION_CONTEXT **)dec->above_context_alloc[3];
  cm->above_txfm_context = (TXFM_CONTEXT **)dec->above_context_alloc[4];

  cm->rst_info[0].unit_info = (RestorationUnitInfo *)dec->restoration_info_alloc[0];
  cm->rst_info[1].unit_info = (RestorationUnitInfo *)dec->restoration_info_alloc[1];
  cm->rst_info[2].unit_info = (RestorationUnitInfo *)dec->restoration_info_alloc[2];
  cm->tpl_mvs = (TPL_MV_REF *)dec->tplmvs_alloc;
}

extern "C" void av1_show_frame(AV1Decoder *pbi, YV12_BUFFER_CONFIG *buf, int is_visible) {}

void copy_to_img(Av1Core *dec, HwOutputImage *hw_img, aom_image_t *dst) {
  dst->bit_depth = hw_img->hbd ? 10 : 8;
  dst->w = hw_img->y_crop_width;
  dst->h = hw_img->y_crop_height;
  dst->d_w = hw_img->y_crop_width;
  dst->d_h = hw_img->y_crop_height;
  dst->r_w = hw_img->y_crop_width;
  dst->r_h = hw_img->y_crop_height;
  dst->x_chroma_shift = 1;
  dst->y_chroma_shift = 1;
  dst->planes[AOM_PLANE_Y] = hw_img->fb_ptr + hw_img->planes[0].offset;
  dst->planes[AOM_PLANE_U] = hw_img->fb_ptr + hw_img->planes[1].offset;
  dst->planes[AOM_PLANE_V] = hw_img->fb_ptr + hw_img->planes[2].offset;
  dst->stride[AOM_PLANE_Y] = hw_img->planes[0].stride;
  dst->stride[AOM_PLANE_U] = hw_img->planes[1].stride;
  dst->stride[AOM_PLANE_V] = hw_img->planes[2].stride;
  dst->user_priv = hw_img->user_priv;
  dst->fb2_priv = hw_img->alloc_priv;
  dst->monochrome = hw_img->monochrome;
  if (hw_img->monochrome) {
    dst->planes[AOM_PLANE_U] = NULL;
    dst->planes[AOM_PLANE_V] = NULL;
  }
  dst->is_hdr10x3 = dec->tryhdr10x3;
  hw_img->fb_ptr = NULL;
  hw_img->alloc_priv = NULL;
  hw_img->user_priv = NULL;
  hw_img->is_valid = 0;
}

extern "C" int get_output_frame(AV1Decoder *pbi, aom_image_t *dst) {
  Av1Core *dec = pbi->gpu_decoder;
  if (MTQueueIsEmpty(&dec->output_queue)) return 1;
  HwOutputImage *hw_img = (HwOutputImage *)MTQueueGet(&dec->output_queue);
  copy_to_img(dec, hw_img, dst);
  MTQueuePush(&dec->image_pool, hw_img);
  return 0;
}

void av1_sync_gpu(av1_frame_thread_data *td) {
  ComputeCommandBuffer *cb = &td->command_buffer;
  WaitForSingleObject(cb->event, INFINITE);
}

int release_fb(Av1Core *dec, HwFrameBuffer *fb) {
  --fb->ref_cnt;
  if (fb->ref_cnt == 0) {
    QueuePush(&dec->fb_pool, fb);
    return 1;
  }
  return 0;
}

THREADFN gpu_thread_hook(void *data) {
  Av1Core *dec = (Av1Core *)data;
  while (1) {
    GpuWorkItem *item = (GpuWorkItem *)MTQueueGet(&dec->gpu_waiting_queue);
    if (!item) {
      if (dec->cb_notify_frame_ready && dec->image_alloc_priv) dec->cb_notify_frame_ready(dec->image_alloc_priv, 0);
      break;
    }

    av1_frame_thread_data *td = item->data;
    av1_perf_frame perf_rec = {};
    int perf_have_rec = 0;
    if (td) {
      // sync gpu
      const int64_t perf_wait_start = td->perf.collect ? perf_now() : 0;
      av1_sync_gpu(td);
      // -dumpstages: the copies of the frame's stages, while the slot is still this frame's.
      if (td->dump_count) stage_dump_deliver(td);
      if (perf_wait_start) {
        // Read the frame's timestamps now: its fence is done and the slot has
        // not gone back to the pool yet.
        perf_collect(dec, td, perf_wait_start, &perf_rec);
        perf_have_rec = 1;
      }
      pthread_mutex_lock(&dec->fb_pool_mutex);
      int signal = 0;
      if (td->back_buffer0) signal |= release_fb(dec, td->back_buffer0);
      signal |= release_fb(dec, td->dst_frame_buffer);
      for (int r = 0; r < 7; ++r) {
        if (td->refs[r]) signal |= release_fb(dec, td->refs[r]);
        td->refs[r] = 0;
      }
      td->dst_frame_buffer = 0;
      td->back_buffer0 = 0;
      if (signal) {
        pthread_cond_signal(&dec->fb_pool_empty_cond);
      }
      if (td->sec_thread_data) {
        MTQueuePush(&dec->frame_data_pool, td->sec_thread_data);
      }
      pthread_mutex_unlock(&dec->fb_pool_mutex);
      td->sec_thread_data = NULL;
      // -asyncclear: clear the slot's grids here instead of on the decode thread. A second slot went back
      // above as it is: a frame uses only the coefficient and mode info shares of its second slot, not the
      // grids, so that slot is as clean as it was.
      const int64_t perf_clear_start = perf_have_rec ? perf_now() : 0;
      const size_t cleared = async_clear_slot(td);
      if (perf_clear_start && cleared) {
        LARGE_INTEGER qpf;
        QueryPerformanceFrequency(&qpf);
        perf_rec.cpu_ms[AV1_PERF_CPU_ASYNC_CLEAR] =
            (double)(perf_now() - perf_clear_start) * 1000.0 / (double)qpf.QuadPart;
        perf_rec.async_clear_bytes = (int64_t)cleared;
      }
      // A pipeline slot, or the output context of a show_existing_frame copy.
      MTQueuePush(td->output_only ? &dec->output_data_pool : &dec->frame_data_pool, td);
    }

    HwOutputImage *hw_img = item->image;
    if (hw_img) {
      if (dec->cb_notify_frame_ready && dec->image_alloc_priv) {
        aom_image_t img = {};
        aom_image_t *dst = &img;
        copy_to_img(dec, hw_img, dst);
        MTQueuePush(&dec->image_pool, hw_img);
        const int64_t perf_notify_start = perf_have_rec ? perf_now() : 0;
        dec->cb_notify_frame_ready(dec->image_alloc_priv, dst);
        if (perf_notify_start) {
          LARGE_INTEGER qpf;
          QueryPerformanceFrequency(&qpf);
          perf_rec.cpu_ms[AV1_PERF_CPU_NOTIFY] = (double)(perf_now() - perf_notify_start) * 1000.0 / (double)qpf.QuadPart;
        }
      } else {
        MTQueuePush(&dec->output_queue, hw_img);
      }
    }
    if (perf_have_rec) {
      const av1_perf_frame_cb_fn_t perf_cb = g_perf_cb;
      if (perf_cb) perf_cb(g_perf_priv, &perf_rec);
    }
    MTQueuePush(&dec->gpu_item_pool, item);
  }

  return THREAD_RETURN(0);
}

// Whether the frames of a sequence fit the buffers the decoder was created with: its largest frame, rounded
// up to 128 as the buffers are, and its bit depth (frame buffers of one byte a sample hold 8-bit frames only;
// those of two bytes hold both). The sizes the decoder has go to |dec_*| for the message.
extern "C" int av1_sequence_fits_decoder(Av1Core *dec, int max_frame_width, int max_frame_height, int bit_depth,
                                         int *dec_width, int *dec_height, int *dec_bitdepth) {
  if (dec_width) *dec_width = dec->max_width;
  if (dec_height) *dec_height = dec->max_height;
  if (dec_bitdepth) *dec_bitdepth = dec->max_bitdepth;
  return ((max_frame_width + 127) & ~127) <= dec->max_width && ((max_frame_height + 127) & ~127) <= dec->max_height &&
         (bit_depth <= 8 || dec->max_bitdepth > 8);
}

extern "C" int av1_reallocate_frame_buffer(void *priv, YV12_BUFFER_CONFIG *ybf, int width, int height,
                                           int upscaled_width, int hbd) {
  if (!ybf || !priv) return -1;

  Av1Core *dec = (Av1Core *)priv;
  ybf->hw_show_image = NULL;
  if (!ybf->hw_buffer) {
    ybf->hw_buffer = get_frame_buffer(dec);
  }

  if (!ybf->hw_buffer) return -1;
  const int bpp = hbd + 1;
  const int h_border = 16;
  const int aligned_width = (width + 127) & ~127;
  const int aligned_height = (height + 127) & ~127;
  const int y_stride = aligned_width + 2 * h_border;
  const int y_height = aligned_height;
  const int y_size = y_height * y_stride;
  const int uv_stride = (aligned_width >> 1) + 2 * h_border;
  const int uv_size = (y_height >> 1) * uv_stride;
  const int fb_size = (y_size + 2 * uv_size) * (1 + hbd);

  const int superres_w = (upscaled_width + 127) & ~127;
  const int superres_size =
      (y_height * (superres_w + 2 * h_border) + 2 * (y_height >> 1) * ((superres_w >> 1) + 2 * h_border)) * (1 + hbd);
  // The frame buffer is not the only buffer sized at creation: the per-column and per-row tables, the block
  // lists and the grids are sized for the width and the height the decoder was created with. A frame that is
  // wider or higher than that is refused even when its bytes would fit a frame buffer.
  if (ybf->hw_buffer->size < fb_size || ybf->hw_buffer->size < superres_size ||
      aligned_width > dec->max_width || superres_w > dec->max_width || aligned_height > dec->max_height ||
      (width != upscaled_width && dec->enable_superres == 0)) {
    frame_buffer_release(dec, ybf->hw_buffer);
    ybf->hw_buffer = NULL;
    return -1;
  }

  ybf->buffer_alloc = NULL;
  ybf->buffer_alloc_sz = 0;
  ybf->y_crop_width = width;
  ybf->y_crop_height = height;
  ybf->y_width = (width + 7) & ~7;
  ybf->y_height = (height + 7) & ~7;
  ybf->y_stride = y_stride;

  ybf->uv_crop_width = (width + 1) >> 1;
  ybf->uv_crop_height = (height + 1) >> 1;
  ybf->uv_width = ybf->y_width >> 1;
  ybf->uv_height = ybf->y_height >> 1;
  ybf->uv_stride = uv_stride;

  ybf->border = h_border;
  ybf->frame_size = (size_t)fb_size;
  ybf->subsampling_x = 1;
  ybf->subsampling_y = 1;
  ybf->use_external_reference_buffers = 0;
  ybf->flags = hbd ? YV12_FLAG_HIGHBITDEPTH : 0;

  ybf->y_buffer = NULL;
  ybf->u_buffer = NULL;
  ybf->v_buffer = NULL;
  HwFrameBuffer *buf = ybf->hw_buffer;
  buf->width = ybf->y_width;
  buf->height = ybf->y_height;
  buf->hbd = hbd;
  buf->y_crop_width = ybf->y_crop_width;
  buf->y_crop_height = ybf->y_crop_height;
  buf->uv_crop_width = ybf->uv_crop_width;
  buf->uv_crop_height = ybf->uv_crop_height;
  buf->planes[0].stride = ybf->y_stride * bpp;
  buf->planes[1].stride = ybf->uv_stride * bpp;
  buf->planes[2].stride = ybf->uv_stride * bpp;
  buf->planes[0].offset = static_cast<int>(buf->base_offset + h_border * bpp);
  buf->planes[1].offset = static_cast<int>(buf->base_offset + (y_size + h_border) * bpp);
  buf->planes[2].offset = static_cast<int>(buf->planes[1].offset + uv_size * bpp);
  buf->planes[0].res_stride = sizeof(short) * ((ybf->y_width + 127) & (~127));
  buf->planes[1].res_stride = sizeof(short) * ((ybf->uv_width + 127) & (~127));
  buf->planes[2].res_stride = sizeof(short) * ((ybf->uv_width + 127) & (~127));
  buf->planes[0].res_offset = 0;
  buf->planes[1].res_offset = buf->planes[0].res_stride * ((ybf->y_height + 127) & (~127));
  buf->planes[2].res_offset = buf->planes[1].res_offset + buf->planes[1].res_stride * ((ybf->uv_height + 127) & (~127));
  return 0;
}

void av1_release_fb_callback(void *priv, YV12_BUFFER_CONFIG *ybf) {
  Av1Core *dec = (Av1Core *)priv;

  if (ybf->hw_buffer) {
    frame_buffer_release(dec, ybf->hw_buffer);
    ybf->hw_buffer = NULL;
  }
  HwOutputImage *img = ybf->hw_show_image;
  ybf->hw_show_image = NULL;
  if (img) {
    dec->cb_release_image(dec->image_alloc_priv, img->alloc_priv);
    img->fb_ptr = NULL;
    img->size = 0;
    img->alloc_priv = NULL;
    img->is_valid = 0;
    MTQueuePush(&dec->image_pool, img);
  }
}

void av1_drain_gpu_decoder(Av1Core *dec) {
  if (dec && dec->gpu_thread) {
    MTQueuePush(&dec->gpu_waiting_queue, NULL);
    pthread_join(dec->gpu_thread, 0);
    dec->gpu_thread = NULL;
  }
}

void get_sec_data(Av1Core *dec, av1_frame_thread_data *td) {
  pthread_mutex_lock(&td->sec_data_mutex);
  if (td->sec_thread_data == NULL) {
    const int64_t perf_t = av1_perf_ticks();
    td->sec_thread_data = (av1_frame_thread_data *)MTQueueGet(&dec->frame_data_pool);
    av1_perf_add(dec, AV1_PERF_CPU_WAIT_SEC_SLOT, perf_t);
    if (perf_t) dec->perf_cur.used_sec_slot = 1;
    av1_frame_thread_data *sec = td->sec_thread_data;
    for (int i = 0; i < td->tile_count; ++i) sec->tile_data[i].mi_count = 0;
  }
  pthread_mutex_unlock(&td->sec_data_mutex);
}

// Test player switch (-intrav2): intra prediction through intra_v2.hlsl, which writes the same bytes with one
// thread group per 64x64 region and far fewer dispatches. Off unless av1_set_intra_v2(1) was called.
static int g_intra_v2 = 0;

extern "C" void av1_set_intra_v2(int enable) { g_intra_v2 = enable; }

// Decides whether the frame takes the -intrav2 path and gets its schedule ready. Returns 0 when the frame
// stays on the original shaders: flag off, intra block copy (not ported), or a frame the buffers were not
// sized for.
static int intra_v2_setup_frame(Av1Core *dec, AV1_COMMON *cm, av1_frame_thread_data *td) {
  // No buffers: the decoder was not created for this path (aom_codec_dec_cfg_t::intra_v2).
  if (!g_intra_v2 || cm->allow_intrabc || !td->intra_v2_buffer || !td->intra_v2_regions) return 0;
  const int cols = (cm->mi_cols + 31) & ~31;  // 4x4 cells, padded to 128 pixels: blocks may reach that far
  const int rows = (cm->mi_rows + 31) & ~31;
  const int cells = cols * rows;
  if (cells + (cells >> 2) > td->intra_v2_cells || (cols >> 4) > IntraV2MaxRegions ||
      (rows >> 4) > IntraV2MaxRegions)
    return 0;
  IntraV2Sched *s = &td->intra_v2_sched;
  s->cells = (uint32_t *)td->intra_v2_buffer->host_ptr;
  s->regions = td->intra_v2_regions;
  s->list = (uint64_t *)(s->cells + 2 * (size_t)(cells + (cells >> 2)));
  s->list_count = &td->intra_v2_list_count;
  s->map_stride = cols;
  s->map_uv_offset = cells;
  s->regions_stride = cols >> 4;
  // Entries of earlier frames stay in the cell map and are told apart by the stamp, so the map is only
  // cleared when the 8-bit stamp starts over (every 255 frames of this slot) or the layout changes.
  int clear = intra_v2_next_stamp(&s->stamp);
  if (cols != td->intra_v2_cols || rows != td->intra_v2_rows) {
    td->intra_v2_cols = cols;
    td->intra_v2_rows = rows;
    s->stamp = 1;
    clear = 1;
  }
  if (clear) {
    const int64_t perf_t = av1_perf_ticks();
    const size_t bytes = (size_t)(cells + (cells >> 2)) * 8;
    memset(s->cells, 0, bytes);
    memset(s->regions, 0, sizeof(int) * 2 * (size_t)(cells >> 8));
    perf_add_memset(dec, AV1_PERF_CPU_MEMSET, perf_t, bytes + sizeof(int) * 2 * (size_t)(cells >> 8));
  }
  td->intra_v2_list_count = 0;
  return 1;
}

// The slot's palette buffer: the pixels of the frame's palette blocks, written by the entropy decoder in the
// layout of a frame buffer and copied into the frame by reconstruct_block.hlsl. A block can use a palette only
// in a frame that allows screen content tools (av1_allow_palette), so the buffer is created when the first
// such frame gets the slot and kept from then on. Returns 0 when the frame has what it needs. Decode thread.
static int palette_buffer_for_frame(Av1Core *dec, const AV1_COMMON *cm, av1_frame_thread_data *td) {
  if (td->palette_buffer || !cm->allow_screen_content_tools) return 0;
  const unsigned long long size = dec->palette_size;
  if (dec->memory_available_cb) {
    const int64_t available = dec->memory_available_cb(dec->memory_available_priv);
    if (available >= 0 && size + AV1_MEMORY_GUARD_MARGIN_BUFFER > (unsigned long long)available) {
      av1_log(1,
              "MEMORY REFUSED palette buffer of slot %d (frame %d allows screen content tools): %llu bytes "
              "(%.1f MiB) and the margin of %.1f MiB do not fit the %.1f MiB the application has left for the "
              "decoder. The frame is a decode error; nothing was allocated and the D3D12 device is intact",
              td->slot_index, td->frame_number, size, to_mib(size), to_mib(AV1_MEMORY_GUARD_MARGIN_BUFFER),
              to_mib((unsigned long long)available));
      return -1;
    }
  }
  // A failure is logged by the allocator as "MEMORY FAILED buffer palette_buffer of slot n".
  td->palette_buffer = dec->memory->create_buffer((size_t)size, MemoryType::DeviceUpload, "palette_buffer", td->slot_index);
  if (!td->palette_buffer) return -1;
  const unsigned long long total = dec->memory->device_bytes();
  av1_log(0,
          "MEMORY palette buffer of slot %d created for frame %d, the first on the slot that allows screen "
          "content tools: %.1f MiB; device buffers now %.1f MiB in %d buffers",
          td->slot_index, td->frame_number, to_mib(size), to_mib(total), dec->memory->buffer_count());
  return 0;
}

// A frame that was set up and never submitted (a decode error between its setup and its submit) still has
// its slot, or its output context, in curr_frame_data, with the frame buffers it counted. They are taken back
// here, before the next frame or show_existing_frame takes a slot or a context of its own. Decode thread.
static void release_unsubmitted_frame(Av1Core *dec) {
  av1_frame_thread_data *td = dec->curr_frame_data;
  if (!td) return;
  pthread_mutex_lock(&dec->fb_pool_mutex);
  if (td->back_buffer0) release_fb(dec, td->back_buffer0);
  if (td->dst_frame_buffer) release_fb(dec, td->dst_frame_buffer);
  for (int r = 0; r < 7; ++r) {
    if (td->refs[r]) release_fb(dec, td->refs[r]);
    td->refs[r] = 0;
  }
  td->dst_frame_buffer = 0;
  td->back_buffer0 = 0;
  pthread_mutex_unlock(&dec->fb_pool_mutex);
  dec->curr_frame_data = NULL;
  if (td->output_only) {
    // The output copy of a show_existing_frame: nothing but the reference to the frame it was to copy.
    MTQueuePush(&dec->output_data_pool, td);
    return;
  }
  // The coefficients of a submitted frame are zeroed by the IDCT shaders as they read them (the entropy
  // decoder writes only the non-zero ones and relies on the rest being zero). This frame never reached the
  // GPU, so its share is cleared here, and the share of its second slot, which it may have written too.
  int *coef_buf = (int *)dec->idct_coefs->host_ptr;
  memset(coef_buf + td->coef_buffer_offset, 0, td->coef_buffer_size);
  if (td->sec_thread_data) {
    memset(coef_buf + td->sec_thread_data->coef_buffer_offset, 0, td->sec_thread_data->coef_buffer_size);
    MTQueuePush(&dec->frame_data_pool, td->sec_thread_data);
    td->sec_thread_data = NULL;
  }
  // Its grids stay dirty and are not marked clean, so the next frame on this slot clears them itself, with or
  // without -asyncclear.
  td->clear_pending = 0;
  MTQueuePush(&dec->frame_data_pool, td);
}

extern "C" void av1_setup_frame(AV1Decoder *pbi, AV1_COMMON *cm) {
  Av1Core *dec = pbi->gpu_decoder;
  const int64_t perf_setup_start = av1_perf_ticks();
  if (perf_setup_start) dec->perf_cur.t_frame_start = perf_setup_start;
  int64_t perf_t = 0;

  release_unsubmitted_frame(dec);

  perf_t = av1_perf_ticks();
  av1_frame_thread_data *td = (av1_frame_thread_data *)MTQueueGet(&dec->frame_data_pool);
  av1_perf_add(dec, AV1_PERF_CPU_WAIT_SLOT, perf_t);
  td->sec_thread_data = NULL;
  dec->curr_frame_data = td;
  td->frame_number = dec->frame_number;

  YV12_BUFFER_CONFIG *buf = &cm->cur_frame->buf;
  buf->hw_show_image = NULL;

  cm->mi_grid_base = (MB_MODE_INFO **)td->mode_info_grid->host_ptr;
  cm->mi_grid_visible = cm->mi_grid_base;
  cm->mi_alloc_size = cm->mi_stride * ((cm->mi_rows + 31) & ~31);
  // Test player flag -asyncclear. The two per-frame clears of the slot (this grid and the intra iteration
  // grid below) are sized from this frame, which the completion thread cannot know when it returns the slot.
  // The rule: after a frame's fence the completion thread clears exactly the bytes that frame's setup
  // cleared, and records the two sizes in the slot (clean_*). A frame skips a clear only when the recorded
  // size is exactly the size it would clear itself; then every byte of the grid is what the clear on this
  // thread would have left, inside the range and past it. Any other case (first use of the slot, another
  // frame size, a slot that went back without a completed frame, the flag off) clears here as before. The
  // marks are taken off at once: from here on the grids are being written.
  const int async_clear = g_async_clear;
  const size_t mi_grid_bytes = sizeof(MB_MODE_INFO **) * cm->mi_alloc_size;
  if (!async_clear || td->clean_mi_bytes != mi_grid_bytes) {
    perf_t = av1_perf_ticks();
    memset(cm->mi_grid_base, 0, mi_grid_bytes);
    perf_add_memset(dec, AV1_PERF_CPU_MEMSET, perf_t, mi_grid_bytes);
  }
  const size_t clean_iter_bytes = td->clean_iter_bytes;
  td->clean_mi_bytes = 0;
  td->clean_iter_bytes = 0;
  td->async_clear = async_clear;
  td->clear_mi_bytes = mi_grid_bytes;
  td->clear_iter_bytes = 0;
  td->clear_pending = 1;

  const int grid_w = (((buf->y_width + 63) & (~63)) >> 2) + 2 + 128;
  const int grid_h = (((buf->y_height + 63) & (~63)) >> 2) + 2 + 128;
  td->iter_grid_stride = grid_w;
  td->iter_grid_stride_uv = grid_w >> 1;
  td->bitdepth = buf->bit_depth;

  td->tile_count = cm->tile_cols * cm->tile_rows;
  dec->thread_count = AOMMIN(pbi->max_threads, td->tile_count);
  // Inter breakdown (-gputimedetail): the tile threads count the blocks they push, each tile into its own
  // counters, which av1_prediction_run_all() adds up when the frame is recorded.
  dec->perf_inter_mode = perf_enabled() ? g_perf_detail : 0;
  if (dec->perf_inter_mode) {
    if (dec->perf_inter_tiles_cap < td->tile_count) {
      free(dec->perf_inter_tiles);
      dec->perf_inter_tiles = (av1_perf_inter_tile *)malloc(sizeof(av1_perf_inter_tile) * (size_t)td->tile_count);
      dec->perf_inter_tiles_cap = dec->perf_inter_tiles ? td->tile_count : 0;
    }
    if (dec->perf_inter_tiles)
      memset(dec->perf_inter_tiles, 0, sizeof(av1_perf_inter_tile) * (size_t)td->tile_count);
    else
      dec->perf_inter_mode = 0;
  }

  td->gen_intra_iter_y = (int *)td->gen_intra_inter_grid->host_ptr;
  td->iter_grid_offset_uv = grid_w * grid_h;
  td->gen_intra_iter_uv = td->gen_intra_iter_y + td->iter_grid_offset_uv;
  memset(td->intra_stats, 0, sizeof(td->intra_stats));
  td->intra_v2 = intra_v2_setup_frame(dec, cm, td);
  if (!td->intra_v2) {
    // With -intrav2 nothing scans the iteration grid; av1_mi_push_block() writes the one entry per block
    // that gen_pred_blocks.hlsl reads.
    // The luma grid (4 bytes per entry) and the chroma grid right behind it: one range from the start of
    // the buffer. -asyncclear: skipped when the completion thread left exactly this range cleared.
    const size_t iter_grid_bytes = (size_t)td->iter_grid_offset_uv * 5;
    if (!async_clear || clean_iter_bytes != iter_grid_bytes) {
      perf_t = av1_perf_ticks();
      memset(td->gen_intra_iter_y, -1, td->iter_grid_offset_uv * 4);
      memset(td->gen_intra_iter_uv, -1, td->iter_grid_offset_uv);
      perf_add_memset(dec, AV1_PERF_CPU_MEMSET, perf_t, iter_grid_bytes);
    }
    // With -intrav2 on, the frames that still come here (intra block copy) are the exception and the next
    // frame on the slot most likely does not clear this grid, so it is not cleared ahead for it.
    if (!g_intra_v2) td->clear_iter_bytes = iter_grid_bytes;
  }

  td->is_hbd = buf->bit_depth > 8;
  td->shaders = td->is_hbd ? &dec->shader_lib->shaders_hbd : &dec->shader_lib->shaders_8bit;

  HwFrameBuffer *fb = buf->hw_buffer;
  assert(fb);

  fb->frame_number = dec->frame_number;
  fb->hbd = td->is_hbd;
  ++dec->frame_number;
  td->do_superres = 1 && dec->enable_superres && cm->width != cm->superres_upscaled_width;
  td->do_loop_rest = 1 && (cm->rst_info[0].frame_restoration_type != RESTORE_NONE ||
                           cm->rst_info[1].frame_restoration_type != RESTORE_NONE ||
                           cm->rst_info[2].frame_restoration_type != RESTORE_NONE);
  td->do_cdef = 1 && !cm->skip_loop_filter && !cm->coded_lossless &&
                (cm->cdef_info.cdef_bits || cm->cdef_info.cdef_strengths[0] || cm->cdef_info.cdef_uv_strengths[0]);
  td->do_filmgrain = 1 && cm->film_grain_params.apply_grain;
  td->dst_frame_buffer = fb;
  td->back_buffer0 = NULL;
  if (td->do_loop_rest || td->do_cdef || td->do_superres) {
    // The frame is reconstructed into a back buffer and filtered from there into its own buffer. Only its
    // own command list reads or writes the back buffer, and the command lists of the frames run on the queue
    // one after the other, so one buffer can serve all frames (shared_back_buffer), as back_buffer1 does for
    // the filters: its description is rewritten here for every frame, after the frame before was recorded.
    // Otherwise the frame takes a buffer from the pool and returns it when its GPU work is done.
    HwFrameBuffer *new_buf = &dec->back_buffer0;
    if (!dec->shared_back_buffer) {
      perf_t = av1_perf_ticks();
      new_buf = get_frame_buffer(dec);
      av1_perf_add(dec, AV1_PERF_CPU_WAIT_FB, perf_t);
      td->back_buffer0 = new_buf;
    }
    new_buf->width = fb->width;
    new_buf->height = fb->height;
    new_buf->hbd = td->is_hbd;
    new_buf->y_crop_width = fb->y_crop_width;
    new_buf->uv_crop_width = fb->uv_crop_width;
    new_buf->y_crop_height = fb->y_crop_height;
    new_buf->uv_crop_height = fb->uv_crop_height;
    memcpy(new_buf->planes, fb->planes, sizeof(fb->planes));
    new_buf->planes[0].offset += static_cast<int>(new_buf->base_offset - fb->base_offset);
    new_buf->planes[1].offset += static_cast<int>(new_buf->base_offset - fb->base_offset);
    new_buf->planes[2].offset += static_cast<int>(new_buf->base_offset - fb->base_offset);
    td->frame_buffer = new_buf;
    // back_buffer1 gets the whole description of the frame, like the back buffer above: CDEF writes into it
    // when the frame has superres and no loop restoration, and the upscale then reads its visible width as
    // the width it scales from. (It used to get the layout only; the visible size was whatever an earlier
    // frame had left there, zero at first, and the upscale of such a frame scaled from that width.)
    dec->back_buffer1.width = fb->width;
    dec->back_buffer1.height = fb->height;
    dec->back_buffer1.hbd = td->is_hbd;
    dec->back_buffer1.y_crop_width = fb->y_crop_width;
    dec->back_buffer1.uv_crop_width = fb->uv_crop_width;
    dec->back_buffer1.y_crop_height = fb->y_crop_height;
    dec->back_buffer1.uv_crop_height = fb->uv_crop_height;
    memcpy(dec->back_buffer1.planes, fb->planes, sizeof(fb->planes));
    dec->back_buffer1.planes[0].offset += static_cast<int>(dec->back_buffer1.base_offset - fb->base_offset);
    dec->back_buffer1.planes[1].offset += static_cast<int>(dec->back_buffer1.base_offset - fb->base_offset);
    dec->back_buffer1.planes[2].offset += static_cast<int>(dec->back_buffer1.base_offset - fb->base_offset);
  } else
    td->frame_buffer = td->dst_frame_buffer;

  pthread_mutex_lock(&dec->fb_pool_mutex);
  ++fb->ref_cnt;
  for (int i = 0; i < 7; ++i) {
    td->refs[i] = NULL;
    if (cm->remapped_ref_idx[i] == -1) continue;
    YV12_BUFFER_CONFIG *ref = &cm->ref_frame_map[cm->remapped_ref_idx[i]]->buf;
    if (!ref) continue;
    assert(ref->hw_buffer->size == dec->fb_size);
    td->refs[i] = ref->hw_buffer;
    ++td->refs[i]->ref_cnt;
  }
  pthread_mutex_unlock(&dec->fb_pool_mutex);

  // From here on the slot is in a state the next av1_setup_frame() can take back (the frame's buffers and
  // references are counted), so a failure below is reported as a decode error of this frame.
  if (palette_buffer_for_frame(dec, cm, td))
    aom_internal_error(&cm->error, AOM_CODEC_MEM_ERROR,
                       "No palette buffer for a frame that allows screen content tools");

  td->ext_idct_buffer = 0;
  td->ext_coef_base = td->coef_buffer_offset;
  {
    // Worst-case layout of the coefficients (IdctCoefCountDenum per 4x4 unit, av1_setup_macroblockd) for a
    // frame with a tile of at most TileSbSizeThreshold superblocks: such a tile is too small for the switch
    // to a second share at a superblock boundary.
    const int mib_log2 = cm->seq_params.mib_size_log2;
    int coef_end = 0;
    for (int r = 0; r < cm->tile_rows; ++r)
      for (int c = 0; c < cm->tile_cols; ++c) {
        td->ext_idct_buffer |= ((cm->tile_col_start_sb[c + 1] - cm->tile_col_start_sb[c]) *
                                (cm->tile_row_start_sb[r + 1] - cm->tile_row_start_sb[r])) <= TileSbSizeThreshold;
        const int row_start = cm->tile_row_start_sb[r] << mib_log2;
        const int col_start = cm->tile_col_start_sb[c] << mib_log2;
        const int rows = ((AOMMIN(cm->tile_row_start_sb[r + 1] << mib_log2, cm->mi_rows) - row_start) + 15) & ~15;
        const int cols = ((AOMMIN(cm->tile_col_start_sb[c + 1] << mib_log2, cm->mi_cols) - col_start) + 15) & ~15;
        const int mi_offset = row_start * ((cm->mi_cols + 15) & ~15) + col_start * rows;
        coef_end = AOMMAX(coef_end, (mi_offset + rows * cols) * IdctCoefCountDenum);
      }
    if (td->ext_idct_buffer) {
      get_sec_data(dec, td);
      // The layout runs on from one share into the next. It starts in this slot's share, as it always did,
      // when it fits that share alone or when the second slot's share is the one behind it; when the second
      // slot's share is the one in front, it starts there (it used to start in this slot's share all the
      // same and ran over the share behind it, which is another slot's or the end of the buffer).
      const int share = td->coef_buffer_size / (int)sizeof(int);
      const int sec_offset = td->sec_thread_data->coef_buffer_offset;
      if (coef_end > share) {
        if (coef_end <= 2 * share && sec_offset + share == td->coef_buffer_offset) {
          td->ext_coef_base = sec_offset;
        } else if (coef_end > 2 * share || td->coef_buffer_offset + share != sec_offset) {
          av1_log(1,
                  "MEMORY FAILED frame %d: a tile of at most %d superblocks needs the worst-case coefficient "
                  "layout, %d coefficients, in two shares of %d that follow each other; the frame has the "
                  "shares of slots %d and %d",
                  td->frame_number, (int)TileSbSizeThreshold, coef_end, share, td->slot_index,
                  td->sec_thread_data->slot_index);
          aom_internal_error(&cm->error, AOM_CODEC_MEM_ERROR,
                             "No room for the coefficients of a frame with very small tiles");
        }
      }
    }
  }

  assert(dec->thread_count <= EntropyThreadCount);
  if (!dec->gpu_thread) pthread_create(&dec->gpu_thread, 0, (LPTHREAD_START_ROUTINE)gpu_thread_hook, dec);

  td->scale_enable = 0;
  for (int i = LAST_FRAME; i <= ALTREF_FRAME; ++i) {
    scale_factors *sf = get_ref_scale_factors(cm, i);
    if (!sf) {
      td->scale_factors[i].x_scale = REF_NO_SCALE;
      td->scale_factors[i].y_scale = REF_NO_SCALE;
      td->scale_factors[i].x_step = 0;
      td->scale_factors[i].y_step = 0;
    } else {
      td->scale_factors[i].x_step = sf->x_step_q4;
      td->scale_factors[i].y_step = sf->y_step_q4;
      td->scale_factors[i].x_scale = sf->x_scale_fp;
      td->scale_factors[i].y_scale = sf->y_scale_fp;
      td->scale_enable |= av1_is_scaled(sf);
    }
    // Global motion warp from this reference: the model, and a reference that is not scaled. libaom's
    // av1_allow_warp() refuses the warp of a scaled reference before it looks at the model, and so does the
    // specification (useWarp, 7.11.3.1: is_scaled(refFrame) is 0); the block is then predicted from its
    // motion vector, like a block of a frame without the model. A local warp needs no such test here: a
    // block with a scaled reference cannot be coded as one (motion_mode_allowed of blockd.h).
    const WarpedMotionParams *gm = &cm->global_motion[i];
    td->gm_warp[i] = gm->wmtype > TRANSLATION && !gm->invalid && !(sf && av1_is_scaled(sf));
  }
  // -interfast: not for a frame with a scaled reference, whose blocks all go through the inter_scale shaders.
  td->inter_fast = (dec->inter_fast_tiles > 0 && !td->scale_enable) ? g_inter_fast : 0;
  av1_perf_add(dec, AV1_PERF_CPU_SETUP, perf_setup_start);
}

extern "C" void av1_setup_ext_coef_buffer(AV1Decoder *pbi, AV1_COMMON *cm, ThreadData *thread_data) {
  av1_tile_data *tile = thread_data->tile_data;
  Av1Core *dec = pbi->gpu_decoder;
  av1_frame_thread_data *cur_td = dec->curr_frame_data;
  get_sec_data(dec, cur_td);
  av1_frame_thread_data *td = cur_td->sec_thread_data;
  // The tile's part of the second slot's share: the same place and size as in the first.
  tile->dq_buffer_ptr = td->coef_buffer_offset + tile->dq_buffer_offset;
  tile->dq_buffer_end = tile->dq_buffer_ptr + tile->dq_buffer_size;
}

extern "C" void av1_setup_sec_data(AV1Decoder *pbi, AV1_COMMON *cm, ThreadData *thread_data) {
  av1_tile_data *prev_t = thread_data->tile_data;
  const int tile_id = thread_data->tile_id;
  Av1Core *dec = pbi->gpu_decoder;

  av1_frame_thread_data *cur_td = dec->curr_frame_data;
  get_sec_data(dec, cur_td);

  av1_frame_thread_data *td = cur_td->sec_thread_data;
  av1_tile_data *t = &td->tile_data[tile_id];
  t->mi_count = 0;
  t->mi_offset = prev_t->mi_offset + td->mode_info_offset - cur_td->mode_info_offset;
  thread_data->mi_count2 = 0;
  thread_data->mi_pool2 = ((MB_MODE_INFO *)dec->mode_info_pool->host_ptr) + t->mi_offset;
  thread_data->tile_data2 = t;
}

extern "C" void av1_setup_macroblockd(AV1Decoder *pbi, AV1_COMMON *cm, ThreadData *thread_data, TileInfo *tile) {
  MACROBLOCKD *xd = &thread_data->xd;
  Av1Core *dec = pbi->gpu_decoder;
  const int tile_id = tile->tile_col + tile->tile_row * cm->tile_cols;
  av1_frame_thread_data *td = dec->curr_frame_data;
  av1_tile_data *t = &td->tile_data[tile_id];
  int64_t perf_t = av1_perf_ticks();
  memset(t, 0, sizeof(*t));
  perf_add_memset(dec, AV1_PERF_CPU_MEMSET_TILE, perf_t, sizeof(*t));
  thread_data->tile_data = t;
  xd->tile_data = t;
  if (dec->perf_inter_mode && tile_id < dec->perf_inter_tiles_cap) t->perf_inter = &dec->perf_inter_tiles[tile_id];
  const int mi_offset = tile->mi_row_start * ((cm->mi_cols + 15) & ~15) +
                        tile->mi_col_start * ((tile->mi_row_end - tile->mi_row_start + 15) & (~15));
  const int mi_max =
      ((tile->mi_row_end - tile->mi_row_start + 15) & (~15)) * ((tile->mi_col_end - tile->mi_col_start + 15) & (~15));

  const int coef_num = dec->coef_count_num;
  t->dq_buffer_offset = mi_offset * (td->ext_idct_buffer ? (int)IdctCoefCountDenum : coef_num);
  t->dq_buffer_ptr = (td->ext_idct_buffer ? td->ext_coef_base : td->coef_buffer_offset) + t->dq_buffer_offset;
  t->dq_buffer_base = (tran_low_t *)dec->idct_coefs->host_ptr;
  const int mib_sz = cm->seq_params.mib_size;
  const int sb_sz = mib_sz * mib_sz * IdctCoefCountDenum;
  t->dq_buffer_max = t->dq_buffer_ptr + mi_max * coef_num - sb_sz;
  // The tile's part of a share, and where the part it is writing ends: av1_read_coeffs_txb() does not write
  // behind it. With the worst-case layout the part is as large as the tile can need.
  t->dq_buffer_size = mi_max * coef_num;
  t->dq_buffer_end = t->dq_buffer_ptr + mi_max * (td->ext_idct_buffer ? (int)IdctCoefCountDenum : coef_num);

  const int blocks_4x4_count = (mi_offset * 3 + 1) >> 1;
  t->intra_iter_max = -1;
  t->intra_iter_max_uv = -1;

  t->blocks_offset = blocks_4x4_count;
  t->idct_blocks_host = (tx_block_info_gpu *)td->idct_blocks_unordered->host_ptr + blocks_4x4_count;

  const int mi_offset_base =
      tile->mi_row_start * cm->mi_stride + tile->mi_col_start * (tile->mi_row_end - tile->mi_row_start);
  const int mi_max_base = (tile->mi_row_end - tile->mi_row_start) * (tile->mi_col_end - tile->mi_col_start);
  // The tile's part of the slot's share of the mode info pool: half of its 4x4 units, a quarter with
  // AV1_LOW_MEMORY_MODE_INFO. The part in a second slot's share has the same place and size.
  t->mi_offset = (mi_offset_base >> dec->mode_info_shift) + td->mode_info_offset;
  t->mi_count = 0;
  thread_data->tile_id = tile_id;
  thread_data->mi_count = 0;
  thread_data->mi_count_max = mi_max_base >> dec->mode_info_shift;
  thread_data->mi_pool = ((MB_MODE_INFO *)dec->mode_info_pool->host_ptr) + t->mi_offset;
  thread_data->mi_count2 = 0;
  thread_data->mi_pool2 = NULL;
  thread_data->tile_data2 = NULL;
  thread_data->ext_idct_buffer = td->ext_idct_buffer;

  t->mi_col_start = tile->mi_col_start;
  t->mi_row_start = tile->mi_row_start;
  t->gen_index_ptr = 0;
  t->gen_index_base = blocks_4x4_count * 2;
  t->gen_indexes = ((unsigned int *)td->gen_mi_block_indexes->host_ptr) + t->gen_index_base;
  t->gen_block_warp_offset = tile_id * (256 + 32) + (mi_offset >> 2) * 10;
  t->gen_block_map_offset = t->gen_block_warp_offset + 32;
  t->gen_block_map = ((int *)td->gen_block_map->host_ptr) + t->gen_block_map_offset;
  t->gen_block_map_wrp = ((int *)td->gen_block_map->host_ptr) + t->gen_block_warp_offset;
  // -interfast: the tile's lists by motion vector class, behind the block maps of all tiles.
  if (td->inter_fast && tile_id < dec->inter_fast_tiles) {
    t->gen_block_fast_offset = dec->pred_map_size + tile_id * (int)InterFastMapSize;
    t->gen_block_map_fast = ((int *)td->gen_block_map->host_ptr) + t->gen_block_fast_offset;
    memset(t->gen_block_map_fast, 0, sizeof(int) * InterFastMapSize);
  }
  t->gen_intra_iter_y = -1;
  t->gen_intra_iter_uv = -1;
  t->have_inter = 0;
  t->gen_intra_max_iter = td->tile_count == 1 ? (dec->pred_map_size - 512) / 10
                                              : ((((tile->mi_col_end - tile->mi_col_start + 15) & ~15) *
                                                  ((tile->mi_row_end - tile->mi_row_start + 15) & ~15)) >>
                                                 2) -
                                                    2;
  t->gen_intra_iter_set = AOMMIN(t->gen_intra_max_iter - 16, 256);
  t->gen_iter_clear_offset = (t->gen_intra_iter_set + 16) * 10 + 256;
  t->gen_iter_clear_size = sizeof(int) * (256 + t->gen_intra_max_iter * 10 - t->gen_iter_clear_offset);
  perf_t = av1_perf_ticks();
  memset(t->gen_block_map_wrp, 0, sizeof(int) * (32 + 16 + t->gen_iter_clear_offset));
  perf_add_memset(dec, AV1_PERF_CPU_MEMSET_TILE, perf_t, sizeof(int) * (32 + 16 + t->gen_iter_clear_offset));
}

extern "C" void av1_intra_palette(AV1Decoder *pbi, MB_MODE_INFO *mi, Av1ColorMapParam *params, int plane) {
  Av1Core *dec = pbi->gpu_decoder;
  av1_frame_thread_data *td = dec->curr_frame_data;
  int i;
  const HwFrameBuffer *fb = td->frame_buffer;
  const int bsz = 4 >> plane;
  uint8_t *map = params->color_map;
  // Palette blocks exist only in frames that allow screen content tools, and av1_setup_frame() has created
  // the buffer for those.
  if (!td->palette_buffer) return;
  uint8_t *palette_buf = (uint8_t *)td->palette_buffer->host_ptr;
  if (fb->hbd) {
    const int stride = fb->planes[plane].stride >> 1;
    const int offset = ((mi->mi_row * bsz) & (~3)) * stride + ((mi->mi_col * bsz) & (~3));
    uint16_t *dst = (uint16_t *)(palette_buf + fb->planes[plane].offset - fb->base_offset) + offset;
    for (i = 0; i < params->plane_height; ++i) {
      for (int j = 0; j < params->plane_width; ++j) dst[j] = mi->palette_mode_info.palette_colors[map[j] + 8 * plane];
      dst += stride;
      map += params->plane_width;
    }
    if (plane) {
      dst = (uint16_t *)(palette_buf + fb->planes[2].offset - fb->base_offset) + offset;
      map = params->color_map;
      for (i = 0; i < params->plane_height; ++i) {
        for (int j = 0; j < params->plane_width; ++j) dst[j] = mi->palette_mode_info.palette_colors[map[j] + 16];
        dst += stride;
        map += params->plane_width;
      }
    }
  } else {
    const int stride = fb->planes[plane].stride;
    const int offset = ((mi->mi_row * bsz) & (~3)) * stride + ((mi->mi_col * bsz) & (~3));
    uint8_t *dst = palette_buf + fb->planes[plane].offset - fb->base_offset + offset;
    for (i = 0; i < params->plane_height; ++i) {
      for (int j = 0; j < params->plane_width; ++j)
        dst[j] = static_cast<uint8_t>(mi->palette_mode_info.palette_colors[map[j] + 8 * plane]);
      dst += stride;
      map += params->plane_width;
    }
    if (plane) {
      dst = palette_buf + fb->planes[2].offset - fb->base_offset + offset;  // xd->dev_frame_planes[2] + offset;
      map = params->color_map;
      for (i = 0; i < params->plane_height; ++i) {
        for (int j = 0; j < params->plane_width; ++j)
          dst[j] = static_cast<uint8_t>(mi->palette_mode_info.palette_colors[map[j] + 16]);
        dst += stride;
        map += params->plane_width;
      }
    }
  }
}

// show_existing_frame: the frame in cm->cur_frame (the frame to show; the header reader has put it there,
// set cm->show_frame and loaded the frame's film grain parameters into cm->film_grain_params, the
// load_grain_params of the specification) is output now. The frame was decoded earlier; it took no output
// image then unless it was shown then, and that image went to the application. So the output is made here:
// an output image from the application, and a command list of its own that copies the frame buffer into it,
// with film grain.
//
// Order on the GPU: the command list of the frame's decode was submitted by this thread before this header
// was read, and the lists run on the one queue in the order of their submission, so the copy reads the
// finished frame. Nothing writes a frame buffer after the frame's own list.
// Lifetime: the copy holds a reference to the frame buffer (frame_buffer_acquire, taken while the reference
// map still holds the frame) until the completion thread has seen the fence of the copy, so the buffer
// cannot go back to the pool and be written by a later frame before the copy has run, whatever the stream
// does to the reference map in between.
// Output order: the work item is queued behind those of the frames before it, and the completion thread
// hands the images over in that order.
//
// The copy takes an output context, not a pipeline slot: a slot would be one frame less in flight for as
// long as the copy waits behind the frames in front of it on the queue.
extern "C" void av1_decode_sef(AV1Decoder *pbi) {
  Av1Core *dec = pbi->gpu_decoder;
  AV1_COMMON *cm = &pbi->common;
  YV12_BUFFER_CONFIG *buf = &cm->cur_frame->buf;
  // A frame before this one that failed after its setup.
  release_unsubmitted_frame(dec);
  buf->hw_show_image = NULL;
  // Nothing to show: no frame was ever decoded into this entry.
  if (!buf->hw_buffer) return;
  // Not the frame of its temporal unit that is output: a frame of a higher spatial layer follows.
  if (!av1_frame_is_output(pbi)) return;

  int64_t perf_t = av1_perf_ticks();
  if (perf_t) {
    dec->perf_cur.t_frame_start = perf_t;
    dec->perf_cur.is_sef = 1;
    dec->perf_cur.show_frame = 1;
    dec->perf_cur.intra_iters = -1;
    dec->perf_cur.frame_number = buf->hw_buffer->frame_number;
  }
  // Waits when both contexts are in flight: until the copy of an earlier show_existing_frame has completed.
  av1_frame_thread_data *td = (av1_frame_thread_data *)MTQueueGet(&dec->output_data_pool);
  av1_perf_add(dec, AV1_PERF_CPU_WAIT_SLOT, perf_t);
  td->sec_thread_data = NULL;
  td->back_buffer0 = NULL;
  memset(td->refs, 0, sizeof(td->refs));
  td->clear_pending = 0;
  td->frame_number = buf->hw_buffer->frame_number;
  td->dst_frame_buffer = buf->hw_buffer;
  td->frame_buffer = buf->hw_buffer;
  td->is_hbd = buf->bit_depth > 8;
  td->bitdepth = buf->bit_depth;
  td->shaders = td->is_hbd ? &dec->shader_lib->shaders_hbd : &dec->shader_lib->shaders_8bit;
  td->do_cdef = 0;
  td->do_loop_rest = 0;
  td->do_superres = 0;
  td->do_filmgrain = cm->film_grain_params.apply_grain;
  td->output_frame = 1;
  frame_buffer_acquire(dec, td->dst_frame_buffer);
  // From here on release_unsubmitted_frame() can take the context back if the output image cannot be had.
  dec->curr_frame_data = td;
  // The completion thread ends with a drain; a show_existing_frame may be the first thing after one.
  if (!dec->gpu_thread) pthread_create(&dec->gpu_thread, 0, (LPTHREAD_START_ROUTINE)gpu_thread_hook, dec);

  av1_prepare_command_buffer(dec);
  if (td->dump_active)
    snprintf(td->dump_info, sizeof(td->dump_info), "show_existing_frame film_grain=%d", td->do_filmgrain);
  av1_postprocess_copy_output(dec, cm);
  av1_commit_command_buffer(dec);

  perf_t = av1_perf_ticks();
  GpuWorkItem *item = (GpuWorkItem *)MTQueueGet(&dec->gpu_item_pool);
  av1_perf_add(dec, AV1_PERF_CPU_WAIT_ITEM, perf_t);
  item->data = td;
  item->image = buf->hw_show_image;
  buf->hw_show_image = NULL;
  dec->curr_frame_data = NULL;
  if (item->image) item->image->user_priv = pbi->user_priv;
  MTQueuePush(&dec->gpu_waiting_queue, item);
}

extern "C" int av1_decode_frame_gpu(AV1Decoder *pbi) {
  Av1Core *dec = pbi->gpu_decoder;
  AV1_COMMON *cm = &pbi->common;
  int64_t perf_t = av1_perf_ticks();
  av1_prepare_command_buffer(dec);
  stage_dump_frame_info(dec, cm);
  av1_prediction_gen_blocks(pbi, dec);
  perf_t = perf_lap(dec, AV1_PERF_CPU_REC_GEN_BLOCKS, perf_t);
  av1_idct_run(dec);
  perf_t = perf_lap(dec, AV1_PERF_CPU_REC_IDCT, perf_t);
  av1_prediction_run_all(dec, cm, NULL);
  perf_t = perf_lap(dec, AV1_PERF_CPU_REC_PRED, perf_t);
  av1_loopfilter_gpu(dec, cm, &pbi->mb);
  perf_t = perf_lap(dec, AV1_PERF_CPU_REC_LOOPFILTER, perf_t);
  av1_cdef_looprestoration(dec, cm, &pbi->lr_ctxt);
  perf_t = perf_lap(dec, AV1_PERF_CPU_REC_CDEF_LR, perf_t);
  av1_inter_ext_borders(dec, cm);
  perf_t = perf_lap(dec, AV1_PERF_CPU_REC_BORDERS, perf_t);
  // A shown frame is output, unless it is a lower spatial layer of its temporal unit.
  dec->curr_frame_data->output_frame = cm->show_frame && av1_frame_is_output(pbi);
  av1_postprocess_copy_output(dec, cm);
  perf_t = perf_lap(dec, AV1_PERF_CPU_REC_COPY, perf_t);
  if (perf_t) {
    const av1_frame_thread_data *td = dec->curr_frame_data;
    av1_perf_accum *acc = &dec->perf_cur;
    acc->frame_number = td->frame_number;
    acc->show_frame = cm->show_frame;
    acc->showable_frame = cm->showable_frame;
    acc->intra_frame = frame_is_intra_only(cm);
    acc->intra_iters = td->intra_iters;
    acc->intra_v2 = td->intra_v2;
    memcpy(acc->intra_stats, td->intra_stats, sizeof(acc->intra_stats));
    acc->tile_count = td->tile_count;
  }
  av1_commit_command_buffer(dec);
  perf_t = av1_perf_ticks();
  GpuWorkItem *item = (GpuWorkItem *)MTQueueGet(&dec->gpu_item_pool);
  av1_perf_add(dec, AV1_PERF_CPU_WAIT_ITEM, perf_t);
  item->data = dec->curr_frame_data;
  item->image = NULL;
  dec->curr_frame_data = NULL;
  if (cm->cur_frame->buf.hw_show_image) {  // the frame is output: av1_postprocess_copy_output()
    item->image = cm->cur_frame->buf.hw_show_image;
    item->image->user_priv = pbi->user_priv;
    cm->cur_frame->buf.hw_show_image = NULL;
  }
  MTQueuePush(&dec->gpu_waiting_queue, item);
  return 0;
}
