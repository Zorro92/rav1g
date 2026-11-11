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

#include "dx/types.h"
#include "dx/av1_core.h"
#include "dx/av1_memory.h"
#include "dx/av1_compute.h"
#include "av1\common\scan.h"
#include "av1\common\idct.h"

struct cb_sort_data {
  int count;
  int src_offset;
  int reserved[2];
  int offsets[20][4];
};

struct idct_frame_data {
  int planes[3][4];
  int bitdepth;
};

void av1_idct_run(Av1Core *dec) {
  av1_frame_thread_data *td = dec->curr_frame_data;

  const int tile_count = td->tile_count;
  int offset = 0;
  for (int i = 0; i <= TX_SIZES_ALL + 1; ++i) {
    for (int t = 0; t < tile_count; ++t) {
      int count = td->tile_data[t].idct_blocks_sizes[i];
      td->tile_data[t].idct_blocks_sizes[i] = offset;
      offset += count;
    }
  }

  ComputeCommandBuffer *cb = &td->command_buffer;
  Microsoft::WRL::ComPtr<ID3D12GraphicsCommandList> command_list = dec->compute.command_list;
  ComputeShader *shader = &dec->shader_lib->shader_fill_buffer;

  command_list->SetComputeRootSignature(shader->signaturePtr.Get());
  command_list->SetPipelineState(shader->pso.Get());
  command_list->SetComputeRootUnorderedAccessView(0, dec->idct_residuals->dev->GetGPUVirtualAddress());
  int value[2];
  value[0] = static_cast<int>(dec->idct_residuals->size >> 4);
  value[1] = 0;
  command_list->SetComputeRoot32BitConstants(1, 2, value, 0);
  command_list->Dispatch((value[0] + 63) / 64, 1, 1);
  av1_perf_gpu_mark(dec, AV1_PERF_GPU_RESIDUAL_CLEAR);

  shader = &dec->shader_lib->shader_idct_sort;
  command_list->SetComputeRootSignature(shader->signaturePtr.Get());
  command_list->SetPipelineState(shader->pso.Get());
  command_list->SetComputeRootShaderResourceView(0, td->idct_blocks_unordered->dev->GetGPUVirtualAddress());
  command_list->SetComputeRootUnorderedAccessView(1, dec->idct_blocks->dev->GetGPUVirtualAddress());
  for (int t = 0; t < tile_count; ++t) {
    ConstantBufferObject cbo = cb->Alloc(sizeof(cb_sort_data));
    cb_sort_data *sort_data = reinterpret_cast<cb_sort_data *>(cbo.host_ptr);

    // av1_tile_thread_data * thr = td->thread_data + thread;
    av1_tile_data *tile = td->tile_data + t;
    for (int i = 0; i < TX_SIZES_ALL + 1; ++i) {
      sort_data->offsets[i][0] = tile->idct_blocks_sizes[i];
    }
    sort_data->count = tile->idct_blocks_ptr;
    sort_data->src_offset = tile->blocks_offset;
    if (tile->idct_blocks_ptr) {
      command_list->SetComputeRootConstantBufferView(2, cbo.dev_address);
      command_list->Dispatch((tile->idct_blocks_ptr + 63) / 64, 1, 1);
    }
  }

  D3D12_RESOURCE_BARRIER barriers[] = {CD3DX12_RESOURCE_BARRIER::UAV(dec->idct_blocks->dev),
                                       CD3DX12_RESOURCE_BARRIER::UAV(dec->idct_residuals->dev)};
  command_list->ResourceBarrier(2, barriers);
  av1_perf_gpu_mark(dec, AV1_PERF_GPU_IDCT_SORT);
  ConstantBufferObject cbo = cb->Alloc(sizeof(idct_frame_data));
  idct_frame_data *data = reinterpret_cast<idct_frame_data *>(cbo.host_ptr);
  data->planes[0][0] = td->frame_buffer->planes[0].res_stride;
  data->planes[0][1] = td->frame_buffer->planes[0].res_offset;
  data->planes[1][0] = td->frame_buffer->planes[1].res_stride;
  data->planes[1][1] = td->frame_buffer->planes[1].res_offset;
  data->planes[2][0] = td->frame_buffer->planes[2].res_stride;
  data->planes[2][1] = td->frame_buffer->planes[2].res_offset;
  data->bitdepth = td->bitdepth;
  av1_tile_data *tdata = td->tile_data;

  command_list->SetComputeRootSignature(dec->shader_lib->sig_idct.Get());
  command_list->SetComputeRootShaderResourceView(0, dec->idct_blocks->dev->GetGPUVirtualAddress());
  command_list->SetComputeRootUnorderedAccessView(1, dec->idct_residuals->dev->GetGPUVirtualAddress());
  command_list->SetComputeRootUnorderedAccessView(2, dec->idct_coefs->dev->GetGPUVirtualAddress());
  command_list->SetComputeRootConstantBufferView(4, cbo.dev_address);
  for (int type = 0; type <= TX_SIZES_ALL; ++type) {
    int offset = tdata->idct_blocks_sizes[type];
    int count = tdata->idct_blocks_sizes[type + 1] - offset;
    if (count == 0) continue;

    const int tx_size = type == TX_SIZES_ALL ? TX_4X4 : type;
    const int block_w = tx_size_wide[tx_size];
    const int block_h = tx_size_high[tx_size];
    const int block_size = block_w * block_h;
    const int wi_per_block = AOMMIN(block_w, block_h);

    ConstantBufferObject scan_cbo = cb->Alloc(block_size * 3 * 4);
    int *gpu_scans = reinterpret_cast<int *>(scan_cbo.host_ptr);
    for (int s = 0; s < 3; ++s) {
      int *dst = gpu_scans + s * block_size;
      const int16_t *scan = av1_idct_scans[tx_size][s];
      if (block_w > 32) {
        for (int i = 0; i < 32 * block_h; ++i) {
          int s = scan[i];
          dst[i] = (s & 31) + (s / 32) * 64;
        }
      } else
        for (int i = 0; i < block_size; ++i) dst[i] = scan[i];
    }

    int const_inline[2];
    const_inline[0] = offset;
    const_inline[1] = wi_per_block * count;
    command_list->SetPipelineState(dec->shader_lib->shader_idct[type].pso.Get());
    command_list->SetComputeRootConstantBufferView(3, scan_cbo.dev_address);
    command_list->SetComputeRoot32BitConstants(5, 2, const_inline, 0);
    command_list->Dispatch((const_inline[1] + 63) / 64, 1, 1);
  }
  command_list->ResourceBarrier(1, &CD3DX12_RESOURCE_BARRIER::UAV(dec->idct_residuals->dev));
  av1_perf_gpu_mark(dec, AV1_PERF_GPU_IDCT);
}
