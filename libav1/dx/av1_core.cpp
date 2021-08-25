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
  CHECK_RESULT(dec->idct_residuals, mem->create_buffer(sizeof(short) * residuals_size, MemoryType::DeviceOnly),
               do_assign);
  CHECK_RESULT(dec->idct_blocks, mem->create_buffer(block_count_4x4 * IdctBlockSize, MemoryType::DeviceOnly),
               do_assign);
  CHECK_RESULT(dec->inter_mask_lut, mem->create_buffer(sizeof(wedge_mask_buf) * 2, MemoryType::DeviceOnlyConst),
               do_assign);
  CHECK_RESULT(dec->inter_warp_filter, mem->create_buffer(sizeof(warped_filter) * 2, MemoryType::DeviceOnlyConst),
               do_assign);

  CHECK_RESULT(dec->filmgrain_noise, mem->create_buffer(sizeof(int) * (96 * 96 + 48 * 48 * 2), MemoryType::DeviceOnly),
               do_assign);
  CHECK_RESULT(dec->filmgrain_gaus, mem->create_buffer(sizeof(dx_gaussian_sequence), MemoryType::DeviceOnlyConst),
               do_assign);
  CHECK_RESULT(dec->filmgrain_random_luma, mem->create_buffer(sizeof(int) * (65536 + 1), MemoryType::DeviceOnlyConst),
               do_assign);
  CHECK_RESULT(dec->filmgrain_random_chroma, mem->create_buffer(sizeof(int) * (65536 + 1), MemoryType::DeviceOnlyConst),
               do_assign);

  // frame buffer size:
  const int border = 16;
  const int y_stride = target_width + 2 * border;
  const int y_size = y_stride * target_height;
  const int uv_stride = (target_width >> 1) + border * 2;
  const int uv_size = uv_stride * (target_height >> 1);
  const int bpp = cfg.bitdepth > 8 ? 2 : 1;
  const int fb_size = ((y_size + 2 * uv_size) * bpp + 255) & ~255;

  dec->fb_size = fb_size;
  dec->fb_offset = fb_size;
  dec->enable_superres = cfg.enable_superres;
  CHECK_RESULT(dec->frame_buffer_pool,
               mem->create_buffer(fb_size * cfg.fb_count + dec->fb_offset, MemoryType::DeviceOnly), do_assign);

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
  CHECK_RESULT(dec->prediction_blocks, mem->create_buffer(block_count_4x4 * 16 * 2, MemoryType::DeviceOnly), do_assign);
  CHECK_RESULT(dec->prediction_blocks_warp, mem->create_buffer(block_count_8x8 * 48, MemoryType::DeviceOnly),
               do_assign);
  CHECK_RESULT(dec->loopfilter_blocks, mem->create_buffer(lf_blk_count * 32, MemoryType::DeviceOnly), do_assign);
  CHECK_RESULT(dec->mode_info_pool, mem->create_buffer(sizeof(MB_MODE_INFO) * mi_size, MemoryType::HostRW), do_assign);
  const int coef_buffer_size = target_width * target_height * 3 * IdctCoefCountNum / (IdctCoefCountDenum * 2);
  CHECK_RESULT(dec->idct_coefs, mem->create_buffer(sizeof(int) * coef_buffer_size * 2, MemoryType::UploadUAV),
               do_assign);
  for (int i = 0; i < cfg.gpu_pipeline_depth; ++i) {
    av1_frame_thread_data *td = &dec->frame_thread_data[i];
    td->mode_info_max = mi_size >> 1;
    td->mode_info_offset = i * (mi_size >> 1);
    td->coef_buffer_offset = i * coef_buffer_size;
    td->coef_buffer_size = coef_buffer_size * sizeof(int);
    CHECK_RESULT(td->command_buffer.cb_alloc, mem->create_buffer(1024 * 1024, MemoryType::DeviceUpload), do_assign);
    CHECK_RESULT(td->tile_data, (av1_tile_data *)mem->host_allocate(sizeof(av1_tile_data) * max_tiles), do_assign);
    CHECK_RESULT(td->gen_mi_block_indexes, mem->create_buffer(sizeof(int) * block_count_4x4 * 2, MemoryType::HostRW),
                 do_assign);
    CHECK_RESULT(td->gen_intra_inter_grid, mem->create_buffer(grid_w * grid_h * 6, MemoryType::HostRW), do_assign);
    CHECK_RESULT(td->gen_block_map, mem->create_buffer(dec->pred_map_size * sizeof(int), MemoryType::HostRW),
                 do_assign);
    CHECK_RESULT(td->mode_info_grid, mem->create_buffer(sizeof(MB_MODE_INFO *) * mi_size, MemoryType::HostRW),
                 do_assign);
    CHECK_RESULT(td->idct_blocks_unordered,
                 mem->create_buffer(block_count_4x4 * IdctBlockSize, MemoryType::DeviceUpload), do_assign);

    CHECK_RESULT(td->cdef_indexes, mem->create_buffer(sizeof(int) * cdef_blocks, MemoryType::DeviceUpload), do_assign);
    CHECK_RESULT(td->cdef_skips, mem->create_buffer(sizeof(int) * cdef_blocks * 16 * 16, MemoryType::DeviceUpload),
                 do_assign);
    CHECK_RESULT(td->loop_rest_types, mem->create_buffer(16 * (block_count_4x4 >> 8), MemoryType::DeviceUpload),
                 do_assign);
    CHECK_RESULT(td->loop_rest_wiener, mem->create_buffer(64 * (block_count_4x4 >> 8), MemoryType::DeviceUpload),
                 do_assign);
    CHECK_RESULT(td->filmgrain_rand_offset,
                 mem->create_buffer(sizeof(int) * ((target_width / 32 + 4) * (target_height / 32 + 1)),
                                    MemoryType::DeviceUpload),
                 do_assign);

    CHECK_RESULT(td->palette_buffer, mem->create_buffer(fb_size, MemoryType::DeviceUpload), do_assign);
  }

  const int mvs_size = (target_width >> 3) * (target_height >> 3) * sizeof(MV_REF);
  const int seg_size = (target_width >> 2) * (target_height >> 2);
  for (int i = 0; i < cfg.ref_fb_count; ++i) {
    HwFrameBuffer *fb = &dec->fb_pool_src[i];
    CHECK_RESULT(fb->mvs_alloc, mem->host_allocate(mvs_size), do_assign);
    CHECK_RESULT(fb->seg_alloc, (uint8_t *)mem->host_allocate(seg_size), do_assign);
  }
  return 0;
}

extern "C" int av1_query_memory_requirements(aom_codec_dec_cfg_t *cfg) {
  av1_memory_allocator_dummy mem;

  mem.host_allocate(32 * 1024);  // assume aom_codec_alg_priv size, actually ~20kb;
  mem.host_allocate(sizeof(av1_memory_allocator));
  mem.host_allocate(sizeof(Av1Core));
  resource_config rcfg;
  rcfg.bitdepth = cfg->bitdepth;
  rcfg.width = cfg->width;
  rcfg.height = cfg->height;
  rcfg.ref_fb_count = 12;
  rcfg.fb_count = rcfg.ref_fb_count;
  rcfg.gpu_pipeline_depth = FrameThreadDataCount;
  rcfg.enable_superres = 1;
  Av1Core dec;
  av1_allocate_buffers(&dec, &mem, rcfg, 0);
  cfg->host_size = mem.get_host_size();
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

extern "C" int av1_create_gpu_decoder(Av1Core **gpu_dec, aom_codec_dec_cfg_t *cfg) {
  if (cfg->host_size < sizeof(av1_memory_allocator)) return -1;

  av1_memory_allocator *mem = new (cfg->host_memory) av1_memory_allocator;
  mem->setup((uint8_t *)cfg->host_memory, cfg->host_size);
  mem->host_allocate(sizeof(*mem));
  Av1Core *dec = (Av1Core *)mem->host_allocate(sizeof(Av1Core));
  if (!dec) return -1;

  memset(dec, 0, sizeof(*dec));
  dec->memory = mem;

  dec->compute.device = static_cast<ID3D12Device *>(cfg->dx12device);
  dec->compute.queue = static_cast<ID3D12CommandQueue *>(cfg->dx12command_queue);
  if (FAILED(create_device(&dec->compute))) return -1;
  dx_compute_context *compute = &dec->compute;
  mem->set_dx_context(compute);

  dec->cb_get_output_image = cfg->out_buffers_cb.get_out_buffer_cb;
  dec->cb_release_image = cfg->out_buffers_cb.release_out_buffer_cb;
  dec->cb_notify_frame_ready = cfg->out_buffers_cb.notify_frame_ready_cb;
  dec->image_alloc_priv = cfg->out_buffers_cb.out_buffers_priv;
  dec->shader_lib = static_cast<compute_shader_lib *>(cfg->dxPsos);
  if (!dec->shader_lib) return -1;
  if (wait_shader_create_complete(dec->shader_lib)) return -1;

  resource_config rcfg;
  rcfg.bitdepth = cfg->bitdepth;
  rcfg.width = cfg->width;
  rcfg.height = cfg->height;
  rcfg.ref_fb_count = 12;
  rcfg.fb_count = rcfg.ref_fb_count;
  rcfg.gpu_pipeline_depth = FrameThreadDataCount;
  rcfg.enable_superres = 1;
  if (av1_allocate_buffers(dec, mem, rcfg, 1)) return -1;
  memset(dec->idct_coefs->host_ptr, 0, dec->idct_coefs->size);

  Microsoft::WRL::ComPtr<ID3D12Device> device = compute->device;
  dec->tryhdr10x3 = cfg->tryHDR10x3;
  HRESULT hr;

  MTQueueInit(&dec->frame_data_pool);
  for (int i = 0; i < FrameThreadDataCount; ++i) {
    av1_frame_thread_data *td = &dec->frame_thread_data[i];
    ComputeCommandBuffer *cb = &td->command_buffer;
    hr = device->CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT, IID_PPV_ARGS(&cb->allocator));
    if (FAILED(hr)) return -1;
    cb->fence_value = 0;
    hr = device->CreateFence(cb->fence_value, D3D12_FENCE_FLAG_NONE, IID_PPV_ARGS(&cb->fence));
    if (FAILED(hr)) return -1;
    cb->event = CreateEvent(nullptr, false, false, nullptr);
    td->frame_number = 0;
    pthread_mutex_init(&td->sec_data_mutex, NULL);
    MTQueuePush(&dec->frame_data_pool, td);
  }

  hr = device->CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_DIRECT,
                                 dec->frame_thread_data[0].command_buffer.allocator.Get(), NULL,
                                 IID_PPV_ARGS(&compute->command_list));
  if (FAILED(hr)) return -1;
  if (FAILED(compute->command_list->Close())) return -1;

  if (FAILED(av1_upload_luts(dec))) return -1;

  MTQueueInit(&dec->gpu_item_pool);
  MTQueueInit(&dec->gpu_waiting_queue);
  for (int i = 0; i < 8; ++i) {
    dec->gpu_item_pool_src[i].data = NULL;
    dec->gpu_item_pool_src[i].image = NULL;
    MTQueuePush(&dec->gpu_item_pool, &dec->gpu_item_pool_src[i]);
  }

  MTQueueInit(&dec->output_queue);
  MTQueueInit(&dec->image_pool);
  for (int i = 0; i < ImagePoolSize; ++i) {
    HwOutputImage *img = &dec->image_pool_src[i];
    img->size = 0;
    img->fb_ptr = NULL;
    img->is_valid = 0;
    MTQueuePush(&dec->image_pool, img);
  }

  dec->back_buffer1.size = dec->fb_offset;
  dec->back_buffer1.base_offset = 0;
  QueueInit(&dec->fb_pool);
  for (int i = 0; i < rcfg.ref_fb_count; ++i) {
    const int offset = dec->fb_offset + dec->fb_size * i;
    HwFrameBuffer *fb = &dec->fb_pool_src[i];
    fb->size = dec->fb_size;
    fb->base_offset = offset;
    fb->ref_cnt = 0;
    QueuePush(&dec->fb_pool, fb);
  }

  pthread_cond_init(&dec->fb_pool_empty_cond, NULL);
  pthread_mutex_init(&dec->fb_pool_mutex, NULL);
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

void av1_destroy_gpu_decoder(Av1Core *dec) {
  if (!dec) return;
  av1_drain_gpu_decoder(dec);

  if (dec->cb_release_image) {
    for (int i = 0; i < ImagePoolSize; ++i) {
      HwOutputImage *img = &dec->image_pool_src[i];
      if (img->is_valid) {
        dec->cb_release_image(dec->image_alloc_priv, img->alloc_priv);
      }
    }
  }

  for (int i = 0; i < FrameThreadDataCount; ++i) {
    av1_frame_thread_data *td = &dec->frame_thread_data[i];
    CloseHandle(td->command_buffer.event);
    pthread_mutex_destroy(&td->sec_data_mutex);
  }

  pthread_cond_destroy(&dec->fb_pool_empty_cond);
  pthread_mutex_destroy(&dec->fb_pool_mutex);
  MTQueueDestroy(&dec->output_queue);
  MTQueueDestroy(&dec->image_pool);
  MTQueueDestroy(&dec->gpu_waiting_queue);
  MTQueueDestroy(&dec->gpu_item_pool);
  MTQueueDestroy(&dec->frame_data_pool);

  if (dec->memory) dec->memory->release();

  dec->~Av1Core();
}

void av1_prepare_command_buffer(Av1Core *dec) {
  av1_frame_thread_data *td = dec->curr_frame_data;
  dx_compute_context *compute = &dec->compute;

  td->command_buffer.allocator->Reset();
  compute->command_list->Reset(td->command_buffer.allocator.Get(), NULL);
  td->command_buffer.Reset();

  PutPerfMarker(td, &td->perf_markers[0]);
}

void av1_commit_command_buffer(Av1Core *dec) {
  av1_frame_thread_data *td = dec->curr_frame_data;
  PutPerfMarker(td, &td->perf_markers[15]);

  dx_compute_context *compute = &dec->compute;
  ComputeCommandBuffer *cb = &td->command_buffer;

  compute->command_list->Close();
  ID3D12CommandList *list[] = {compute->command_list.Get()};

  ++cb->fence_value;
  compute->queue->ExecuteCommandLists(1, list);
  compute->queue->Signal(cb->fence.Get(), cb->fence_value);
  cb->fence->SetEventOnCompletion(cb->fence_value, cb->event);
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
    if (td) {
      // sync gpu
      av1_sync_gpu(td);
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
      MTQueuePush(&dec->frame_data_pool, td);
    }

    HwOutputImage *hw_img = item->image;
    if (hw_img) {
      if (dec->cb_notify_frame_ready && dec->image_alloc_priv) {
        aom_image_t img = {};
        aom_image_t *dst = &img;
        copy_to_img(dec, hw_img, dst);
        MTQueuePush(&dec->image_pool, hw_img);
        dec->cb_notify_frame_ready(dec->image_alloc_priv, dst);
      } else {
        MTQueuePush(&dec->output_queue, hw_img);
      }
    }
    MTQueuePush(&dec->gpu_item_pool, item);
  }

  return THREAD_RETURN(0);
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
  if (ybf->hw_buffer->size < fb_size || ybf->hw_buffer->size < superres_size ||
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
    td->sec_thread_data = (av1_frame_thread_data *)MTQueueGet(&dec->frame_data_pool);
    av1_frame_thread_data *sec = td->sec_thread_data;
    for (int i = 0; i < td->tile_count; ++i) sec->tile_data[i].mi_count = 0;
  }
  pthread_mutex_unlock(&td->sec_data_mutex);
}

extern "C" void av1_setup_frame(AV1Decoder *pbi, AV1_COMMON *cm) {
  Av1Core *dec = pbi->gpu_decoder;

  av1_frame_thread_data *td = dec->curr_frame_data;
  if (td) {
    pthread_mutex_lock(&dec->fb_pool_mutex);
    if (td->back_buffer0) release_fb(dec, td->back_buffer0);
    release_fb(dec, td->dst_frame_buffer);
    for (int r = 0; r < 7; ++r) {
      if (td->refs[r]) release_fb(dec, td->refs[r]);
      td->refs[r] = 0;
    }
    td->dst_frame_buffer = 0;
    td->back_buffer0 = 0;
    pthread_mutex_unlock(&dec->fb_pool_mutex);
    if (td->sec_thread_data) {
      MTQueuePush(&dec->frame_data_pool, td->sec_thread_data);
    }
    MTQueuePush(&dec->frame_data_pool, td);
    int * coef_buf = (int *)dec->idct_coefs->host_ptr;
    memset(coef_buf + td->coef_buffer_offset, 0, td->coef_buffer_size);
    dec->curr_frame_data = NULL;
  }

  td = (av1_frame_thread_data *)MTQueueGet(&dec->frame_data_pool);
  td->sec_thread_data = NULL;
  dec->curr_frame_data = td;
  td->frame_number = dec->frame_number;

  YV12_BUFFER_CONFIG *buf = &cm->cur_frame->buf;
  buf->hw_show_image = NULL;

  cm->mi_grid_base = (MB_MODE_INFO **)td->mode_info_grid->host_ptr;
  cm->mi_grid_visible = cm->mi_grid_base;
  cm->mi_alloc_size = cm->mi_stride * ((cm->mi_rows + 31) & ~31);
  memset(cm->mi_grid_base, 0, sizeof(MB_MODE_INFO **) * cm->mi_alloc_size);

  const int grid_w = (((buf->y_width + 63) & (~63)) >> 2) + 2 + 128;
  const int grid_h = (((buf->y_height + 63) & (~63)) >> 2) + 2 + 128;
  td->iter_grid_stride = grid_w;
  td->iter_grid_stride_uv = grid_w >> 1;
  td->bitdepth = buf->bit_depth;

  td->tile_count = cm->tile_cols * cm->tile_rows;
  dec->thread_count = AOMMIN(pbi->max_threads, td->tile_count);

  td->gen_intra_iter_y = (int *)td->gen_intra_inter_grid->host_ptr;
  td->iter_grid_offset_uv = grid_w * grid_h;
  td->gen_intra_iter_uv = td->gen_intra_iter_y + td->iter_grid_offset_uv;
  memset(td->gen_intra_iter_y, -1, td->iter_grid_offset_uv * 4);
  memset(td->gen_intra_iter_uv, -1, td->iter_grid_offset_uv);

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
    HwFrameBuffer *new_buf = get_frame_buffer(dec);
    td->back_buffer0 = new_buf;
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
    td->frame_buffer = td->back_buffer0;
    dec->back_buffer1.width = fb->width;
    dec->back_buffer1.height = fb->height;
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

  td->ext_idct_buffer = 0;
  if (IdctCoefCountNum != IdctCoefCountDenum) {
    for (int r = 0; r < cm->tile_rows; ++r)
      for (int c = 0; c < cm->tile_cols; ++c) {
        td->ext_idct_buffer |= ((cm->tile_col_start_sb[c + 1] - cm->tile_col_start_sb[c]) *
                                (cm->tile_row_start_sb[r + 1] - cm->tile_row_start_sb[r])) <= TileSbSizeThreshold;
      }
    if (td->ext_idct_buffer) {
      get_sec_data(dec, td);
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
  }
}

extern "C" void av1_setup_ext_coef_buffer(AV1Decoder *pbi, AV1_COMMON *cm, ThreadData *thread_data) {
  av1_tile_data *tile = thread_data->tile_data;
  Av1Core *dec = pbi->gpu_decoder;
  av1_frame_thread_data *cur_td = dec->curr_frame_data;
  get_sec_data(dec, cur_td);
  av1_frame_thread_data *td = cur_td->sec_thread_data;
  tile->dq_buffer_ptr = td->coef_buffer_offset + tile->dq_buffer_offset;
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
  memset(t, 0, sizeof(*t));
  thread_data->tile_data = t;
  xd->tile_data = t;
  const int mi_offset = tile->mi_row_start * ((cm->mi_cols + 15) & ~15) +
                        tile->mi_col_start * ((tile->mi_row_end - tile->mi_row_start + 15) & (~15));
  const int mi_max =
      ((tile->mi_row_end - tile->mi_row_start + 15) & (~15)) * ((tile->mi_col_end - tile->mi_col_start + 15) & (~15));

  t->dq_buffer_offset = mi_offset * (td->ext_idct_buffer ? IdctCoefCountDenum : IdctCoefCountNum);
  t->dq_buffer_ptr = td->coef_buffer_offset + t->dq_buffer_offset;
  t->dq_buffer_base = (tran_low_t *)dec->idct_coefs->host_ptr;
  const int mib_sz = cm->seq_params.mib_size;
  const int sb_sz = mib_sz * mib_sz * IdctCoefCountDenum;
  t->dq_buffer_max = t->dq_buffer_ptr + mi_max * IdctCoefCountNum - sb_sz;
  assert(td->ext_idct_buffer || t->dq_buffer_max > t->dq_buffer_ptr);

  const int blocks_4x4_count = (mi_offset * 3 + 1) >> 1;
  t->intra_iter_max = -1;
  t->intra_iter_max_uv = -1;

  t->blocks_offset = blocks_4x4_count;
  t->idct_blocks_host = (tx_block_info_gpu *)td->idct_blocks_unordered->host_ptr + blocks_4x4_count;

  const int mi_offset_base =
      tile->mi_row_start * cm->mi_stride + tile->mi_col_start * (tile->mi_row_end - tile->mi_row_start);
  const int mi_max_base = (tile->mi_row_end - tile->mi_row_start) * (tile->mi_col_end - tile->mi_col_start);
  t->mi_offset = (mi_offset_base >> 1) + td->mode_info_offset;
  t->mi_count = 0;
  thread_data->tile_id = tile_id;
  thread_data->mi_count = 0;
  thread_data->mi_count_max = mi_max_base >> 1;
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
  memset(t->gen_block_map_wrp, 0, sizeof(int) * (32 + 16 + t->gen_iter_clear_offset));
}

extern "C" void av1_intra_palette(AV1Decoder *pbi, MB_MODE_INFO *mi, Av1ColorMapParam *params, int plane) {
  Av1Core *dec = pbi->gpu_decoder;
  av1_frame_thread_data *td = dec->curr_frame_data;
  int i;
  const HwFrameBuffer *fb = td->frame_buffer;
  const int bsz = 4 >> plane;
  uint8_t *map = params->color_map;
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

extern "C" void av1_decode_sef(AV1Decoder *pbi) {
  Av1Core *dec = pbi->gpu_decoder;
  AV1_COMMON *cm = &pbi->common;
  YV12_BUFFER_CONFIG *buf = &cm->cur_frame->buf;
  GpuWorkItem *item = (GpuWorkItem *)MTQueueGet(&dec->gpu_item_pool);
  item->data = NULL;
  item->image = buf->hw_show_image;

  if (item->image == NULL && buf->hw_buffer) {
    av1_frame_thread_data *td = (av1_frame_thread_data *)MTQueueGet(&dec->frame_data_pool);
    td->dst_frame_buffer = buf->hw_buffer;
    td->is_hbd = buf->bit_depth > 8;
    td->shaders = td->is_hbd ? &dec->shader_lib->shaders_hbd : &dec->shader_lib->shaders_8bit;
    td->do_filmgrain = cm->film_grain_params.apply_grain;
    dec->curr_frame_data = td;
    memset(td->refs, 0, sizeof(td->refs));
    td->back_buffer0 = 0;
    frame_buffer_acquire(dec, td->dst_frame_buffer);
    av1_prepare_command_buffer(dec);
    av1_postprocess_copy_output(dec, cm);
    av1_commit_command_buffer(dec);
    item->image = buf->hw_show_image;
    item->data = td;
  }
  dec->curr_frame_data = NULL;
  if (item->image) item->image->user_priv = pbi->user_priv;
  buf->hw_show_image = NULL;
  MTQueuePush(&dec->gpu_waiting_queue, item);
}

extern "C" int av1_decode_frame_gpu(AV1Decoder *pbi) {
  Av1Core *dec = pbi->gpu_decoder;
  AV1_COMMON *cm = &pbi->common;
  av1_prepare_command_buffer(dec);
  av1_prediction_gen_blocks(pbi, dec);
  av1_idct_run(dec);
  av1_prediction_run_all(dec, cm, NULL);
  av1_loopfilter_gpu(dec, cm, &pbi->mb);
  av1_cdef_looprestoration(dec, cm, &pbi->lr_ctxt);
  av1_inter_ext_borders(dec, cm);
  av1_postprocess_copy_output(dec, cm);
  av1_commit_command_buffer(dec);
  GpuWorkItem *item = (GpuWorkItem *)MTQueueGet(&dec->gpu_item_pool);
  item->data = dec->curr_frame_data;
  item->image = NULL;
  dec->curr_frame_data = NULL;
  if (cm->show_frame) {
    item->image = cm->cur_frame->buf.hw_show_image;
    item->image->user_priv = pbi->user_priv;
    cm->cur_frame->buf.hw_show_image = NULL;
  }
  MTQueuePush(&dec->gpu_waiting_queue, item);
  return 0;
}
