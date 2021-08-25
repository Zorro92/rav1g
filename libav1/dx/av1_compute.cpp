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

#include "dx/av1_compute.h"
extern "C" {
#include "av1/common/warped_motion.h"
#include "av1/common/reconinter.h"
};
#include "dx/types.h"
#include <wrl.h>
#include <dxgi1_4.h>
#include <d3dx12.h>

#include "dx/shaders/h/idct4x4.h"
#include "dx/shaders/h/idct4x8.h"
#include "dx/shaders/h/idct4x16.h"
#include "dx/shaders/h/idct8x4.h"
#include "dx/shaders/h/idct8x8.h"
#include "dx/shaders/h/idct8x16.h"
#include "dx/shaders/h/idct8x32.h"
#include "dx/shaders/h/idct16x4.h"
#include "dx/shaders/h/idct16x8.h"
#include "dx/shaders/h/idct16x16.h"
#include "dx/shaders/h/idct16x32.h"
#include "dx/shaders/h/idct16x64.h"
#include "dx/shaders/h/idct32x8.h"
#include "dx/shaders/h/idct32x16.h"
#include "dx/shaders/h/idct32x32.h"
#include "dx/shaders/h/idct32x64.h"
#include "dx/shaders/h/idct64x16.h"
#include "dx/shaders/h/idct64x32.h"
#include "dx/shaders/h/idct64x64.h"
#include "dx/shaders/h/idct_lossless.h"
#include "dx/shaders/h/idct_sort_blocks.h"
#include "dx/shaders/h/fill_buffer.h"
#include "dx/shaders/h/gen_pred_blocks.h"
#include "dx/shaders/h/loop_restoration.h"
#include "dx/shaders/h/cdef_filter.h"
#include "dx/shaders/h/film_grain_filter.h"
#include "dx/shaders/h/film_grain_gen_luma.h"
#include "dx/shaders/h/film_grain_gen_chroma.h"

#include "dx/shaders/h/copy_plane.h"
#include "dx/shaders/h/copy_plane_10bit10x3.h"

#include "dx/shaders/h/inter_2x2chroma.h"
#include "dx/shaders/h/inter_2x2chroma_hbd.h"
#include "dx/shaders/h/inter_compound.h"
#include "dx/shaders/h/inter_compound_2x2.h"
#include "dx/shaders/h/inter_compound_2x2_hbd.h"
#include "dx/shaders/h/inter_compound_diff_chroma.h"
#include "dx/shaders/h/inter_compound_diff_chroma_hbd.h"
#include "dx/shaders/h/inter_compound_diff_luma.h"
#include "dx/shaders/h/inter_compound_diff_luma_hbd.h"
#include "dx/shaders/h/inter_compound_hbd.h"
#include "dx/shaders/h/inter_compound_masked.h"
#include "dx/shaders/h/inter_compound_masked_hbd.h"
#include "dx/shaders/h/inter_ext_borders.h"
#include "dx/shaders/h/inter_ext_borders_hbd.h"
#include "dx/shaders/h/inter_main.h"
#include "dx/shaders/h/inter_main_hbd.h"
#include "dx/shaders/h/inter_obmc_above.h"
#include "dx/shaders/h/inter_obmc_above_hbd.h"
#include "dx/shaders/h/inter_obmc_left.h"
#include "dx/shaders/h/inter_obmc_left_hbd.h"
#include "dx/shaders/h/inter_warp.h"
#include "dx/shaders/h/inter_warp_compound.h"
#include "dx/shaders/h/inter_warp_compound_hbd.h"
#include "dx/shaders/h/inter_warp_hbd.h"
#include "dx/shaders/h/intra_filter.h"
#include "dx/shaders/h/intra_filter_hbd.h"
#include "dx/shaders/h/intra_main.h"
#include "dx/shaders/h/intra_main_hbd.h"

#include "dx/shaders/h/inter_scale.h"
#include "dx/shaders/h/inter_scale_compound.h"
#include "dx/shaders/h/inter_scale_compound_2x2.h"
#include "dx/shaders/h/inter_scale_compound_2x2_hbd.h"
#include "dx/shaders/h/inter_scale_compound_diff_chroma.h"
#include "dx/shaders/h/inter_scale_compound_diff_chroma_hbd.h"
#include "dx/shaders/h/inter_scale_compound_diff_luma.h"
#include "dx/shaders/h/inter_scale_compound_diff_luma_hbd.h"
#include "dx/shaders/h/inter_scale_compound_hbd.h"
#include "dx/shaders/h/inter_scale_compound_masked.h"
#include "dx/shaders/h/inter_scale_compound_masked_hbd.h"
#include "dx/shaders/h/inter_scale_hbd.h"
#include "dx/shaders/h/inter_scale_obmc_above.h"
#include "dx/shaders/h/inter_scale_obmc_above_hbd.h"
#include "dx/shaders/h/inter_scale_obmc_left.h"
#include "dx/shaders/h/inter_scale_obmc_left_hbd.h"
#include "dx/shaders/h/inter_scale_warp_compound.h"
#include "dx/shaders/h/inter_scale_warp_compound_hbd.h"
#include "dx/shaders/h/inter_scale_2x2chroma.h"
#include "dx/shaders/h/inter_scale_2x2chroma_hbd.h"

#include "dx/shaders/h/reconstruct_block.h"
#include "dx/shaders/h/reconstruct_block_hbd.h"

#include "dx/shaders/h/loopfilter_vert.h"
#include "dx/shaders/h/loopfilter_hor.h"
#include "dx/shaders/h/loopfilter_vert_hbd.h"
#include "dx/shaders/h/loopfilter_hor_hbd.h"
#include "dx/shaders/h/gen_lf_blocks_hor.h"
#include "dx/shaders/h/gen_lf_blocks_vert.h"
#include "dx/shaders/h/upscaling.h"

ID3D12RootSignature* create_root_sig(ID3D12Device* device, int Srvs, int Uavs, int Cbs, int InlineSize) {
  CD3DX12_ROOT_SIGNATURE_DESC computeSignatureDesc;
  CD3DX12_ROOT_PARAMETER params[16];
  int count = 0;
  for (int i = 0; i < Srvs; ++i) params[count++].InitAsShaderResourceView(i);
  for (int i = 0; i < Uavs; ++i) params[count++].InitAsUnorderedAccessView(i);
  int cb;
  for (cb = 0; cb < Cbs; ++cb) params[count++].InitAsConstantBufferView(cb);
  if (InlineSize) {
    params[count++].InitAsConstants(InlineSize, cb);
  }
  computeSignatureDesc.Init(count, params);

  ComPtr<ID3DBlob> computeSignatureData;
  ComPtr<ID3DBlob> compilerLog;
  HRESULT hr = D3D12SerializeRootSignature(&computeSignatureDesc, D3D_ROOT_SIGNATURE_VERSION_1, &computeSignatureData,
                                           &compilerLog);
  if (FAILED(hr)) {
    printf((const char*)compilerLog->GetBufferPointer());
    return NULL;
  }

  ID3D12RootSignature* sig = NULL;
  hr = device->CreateRootSignature(0, computeSignatureData->GetBufferPointer(), computeSignatureData->GetBufferSize(),
                                   IID_PPV_ARGS(&sig));
  return SUCCEEDED(hr) ? sig : NULL;
}

HRESULT create_shader(ID3D12Device* device, const BYTE* src, int size, ID3D12RootSignature* sig, ComputeShader* dst) {
  dst->bytecode = src;
  dst->size = size;
  dst->signaturePtr = sig;
  // dst->pso =
  D3D12_COMPUTE_PIPELINE_STATE_DESC pipelineDesc = {};
  pipelineDesc.pRootSignature = sig;
  pipelineDesc.CS.pShaderBytecode = src;
  pipelineDesc.CS.BytecodeLength = size;
  HRESULT hr = device->CreateComputePipelineState(&pipelineDesc, IID_PPV_ARGS(&dst->pso));
  return hr;
}

THREADFN create_shader_thread_hook(void* data) {
  compute_shader_lib* shader_lib = static_cast<compute_shader_lib*>(data);
  while (1) {
    CreateShaderTask* task = (CreateShaderTask*)MTQueueGet(&shader_lib->task_queue);
    if (!task)
      break;
    else {
      shader_lib->create_shader_errors |= create_shader(task->device, task->src, task->size, task->sig, task->dst);
      if (shader_lib->create_shader_errors) return THREAD_RETURN(1);
    }
  }
  return THREAD_RETURN(0);
}

int wait_shader_create_complete(compute_shader_lib* lib) {
  if (!lib) return -1;
  if (!lib->threads)
    return 0;
  else {
    for (int i = 0; i < lib->threads; i++) {
      pthread_join(lib->create_shader_threads[i], 0);
    }
    lib->threads = 0;
    if (lib->create_shader_tasks) delete[] lib->create_shader_tasks;
    lib->create_shader_tasks = 0;
    return lib->create_shader_errors;
  }
}

void* av1_create_pipeline_cache_handle(void* d3d12device, int threads) {
  bool err = 0;
  if (!d3d12device) return 0;
  compute_shader_lib* shader_lib = new (std::nothrow) compute_shader_lib();
  if (!shader_lib) return 0;
  ID3D12Device* _device = static_cast<ID3D12Device*>(d3d12device);

  err |= (shader_lib->sig_idct = create_root_sig(_device, 1, 2, 2, 2)) == NULL;
  err |= (shader_lib->sig_common111 = create_root_sig(_device, 1, 1, 1, 0)) == NULL;
  err |= (shader_lib->sig_common0102 = create_root_sig(_device, 0, 1, 0, 2)) == NULL;
  err |= (shader_lib->sig_common0110 = create_root_sig(_device, 0, 1, 1, 0)) == NULL;
  err |= (shader_lib->sig_copy_plane = create_root_sig(_device, 0, 1, 0, 6)) == NULL;
  err |= (shader_lib->sig_pred_blocks = create_root_sig(_device, 5, 2, 1, 7)) == NULL;
  err |= (shader_lib->sig_intra_pred = create_root_sig(_device, 3, 1, 1, 10)) == NULL;
  err |= (shader_lib->sig_inter_pred = create_root_sig(_device, 5, 1, 1, 4)) == NULL;
  err |= (shader_lib->sig_loop_rest = create_root_sig(_device, 3, 1, 2, 2)) == NULL;
  err |= (shader_lib->sig_cdef_filter = create_root_sig(_device, 2, 1, 1, 0)) == NULL;
  err |= (shader_lib->sig_film_grain = create_root_sig(_device, 3, 1, 1, 0)) == NULL;
  err |= (shader_lib->sig_lf = create_root_sig(_device, 1, 1, 1, 5)) == NULL;
  err |= (shader_lib->sig_lf_gen = create_root_sig(_device, 2, 1, 1, 6)) == NULL;
  err |= (shader_lib->sig_upscale = create_root_sig(_device, 0, 1, 1, 3)) == NULL;
  if (err) return 0;

  bitdepth_dependent_shaders* s8 = &shader_lib->shaders_8bit;
  bitdepth_dependent_shaders* hbd = &shader_lib->shaders_hbd;

  CreateShaderTask create_tasks[] = {
      {_device, idct4x4, sizeof(idct4x4), shader_lib->sig_idct.Get(), &shader_lib->shader_idct[TX_4X4]},
      {_device, idct4x8, sizeof(idct4x8), shader_lib->sig_idct.Get(), &shader_lib->shader_idct[TX_4X8]},
      {_device, idct4x16, sizeof(idct4x16), shader_lib->sig_idct.Get(), &shader_lib->shader_idct[TX_4X16]},
      {_device, idct8x4, sizeof(idct8x4), shader_lib->sig_idct.Get(), &shader_lib->shader_idct[TX_8X4]},
      {_device, idct8x8, sizeof(idct8x8), shader_lib->sig_idct.Get(), &shader_lib->shader_idct[TX_8X8]},
      {_device, idct8x16, sizeof(idct8x16), shader_lib->sig_idct.Get(), &shader_lib->shader_idct[TX_8X16]},
      {_device, idct8x32, sizeof(idct8x32), shader_lib->sig_idct.Get(), &shader_lib->shader_idct[TX_8X32]},
      {_device, idct16x4, sizeof(idct16x4), shader_lib->sig_idct.Get(), &shader_lib->shader_idct[TX_16X4]},
      {_device, idct16x8, sizeof(idct16x8), shader_lib->sig_idct.Get(), &shader_lib->shader_idct[TX_16X8]},
      {_device, idct16x16, sizeof(idct16x16), shader_lib->sig_idct.Get(), &shader_lib->shader_idct[TX_16X16]},
      {_device, idct16x32, sizeof(idct16x32), shader_lib->sig_idct.Get(), &shader_lib->shader_idct[TX_16X32]},
      {_device, idct16x64, sizeof(idct16x64), shader_lib->sig_idct.Get(), &shader_lib->shader_idct[TX_16X64]},
      {_device, idct32x8, sizeof(idct32x8), shader_lib->sig_idct.Get(), &shader_lib->shader_idct[TX_32X8]},
      {_device, idct32x16, sizeof(idct32x16), shader_lib->sig_idct.Get(), &shader_lib->shader_idct[TX_32X16]},
      {_device, idct32x32, sizeof(idct32x32), shader_lib->sig_idct.Get(), &shader_lib->shader_idct[TX_32X32]},
      {_device, idct32x64, sizeof(idct32x64), shader_lib->sig_idct.Get(), &shader_lib->shader_idct[TX_32X64]},
      {_device, idct64x16, sizeof(idct64x16), shader_lib->sig_idct.Get(), &shader_lib->shader_idct[TX_64X16]},
      {_device, idct64x32, sizeof(idct64x32), shader_lib->sig_idct.Get(), &shader_lib->shader_idct[TX_64X32]},
      {_device, idct64x64, sizeof(idct64x64), shader_lib->sig_idct.Get(), &shader_lib->shader_idct[TX_64X64]},
      {_device, idct_lossless, sizeof(idct_lossless), shader_lib->sig_idct.Get(),
       &shader_lib->shader_idct[TX_SIZES_ALL]},
      {_device, idct_sort_blocks, sizeof(idct_sort_blocks), shader_lib->sig_common111.Get(),
       &shader_lib->shader_idct_sort},
      {_device, fill_buffer, sizeof(fill_buffer), shader_lib->sig_common0102.Get(), &shader_lib->shader_fill_buffer},
      {_device, loop_restoration, sizeof(loop_restoration), shader_lib->sig_loop_rest.Get(),
       &shader_lib->shader_loop_rest},
      {_device, film_grain_filter, sizeof(film_grain_filter), shader_lib->sig_film_grain.Get(),
       &shader_lib->shader_filmgrain_filter},
      {_device, film_grain_gen_luma, sizeof(film_grain_gen_luma), shader_lib->sig_film_grain.Get(),
       &shader_lib->shader_filmgrain_luma_gen},
      {_device, film_grain_gen_chroma, sizeof(film_grain_gen_chroma), shader_lib->sig_film_grain.Get(),
       &shader_lib->shader_filmgrain_chroma_gen},
      {_device, cdef_filter, sizeof(cdef_filter), shader_lib->sig_cdef_filter.Get(), &shader_lib->shader_cdef_filter},

      {_device, gen_pred_blocks, sizeof(gen_pred_blocks), shader_lib->sig_pred_blocks.Get(),
       &shader_lib->shader_gen_pred_blocks},
      {_device, gen_lf_blocks_vert, sizeof(gen_lf_blocks_vert), shader_lib->sig_lf_gen.Get(),
       &shader_lib->shader_gen_lf_vert},
      {_device, gen_lf_blocks_hor, sizeof(gen_lf_blocks_hor), shader_lib->sig_lf_gen.Get(),
       &shader_lib->shader_gen_lf_hor},
      {_device, copy_plane, sizeof(copy_plane), shader_lib->sig_copy_plane.Get(), &shader_lib->shader_copy_plane},
      {_device, copy_plane_10bit10x3, sizeof(copy_plane_10bit10x3), shader_lib->sig_copy_plane.Get(),
       &shader_lib->shader_copy_plane_10bit10x3},
      {_device, upscaling, sizeof(upscaling), shader_lib->sig_upscale.Get(), &shader_lib->shader_upscale},

      {_device, inter_2x2chroma, sizeof(inter_2x2chroma), shader_lib->sig_inter_pred.Get(), &s8->inter_2x2},
      {_device, inter_compound, sizeof(inter_compound), shader_lib->sig_inter_pred.Get(), &s8->inter_comp},
      {_device, inter_compound_2x2, sizeof(inter_compound_2x2), shader_lib->sig_inter_pred.Get(), &s8->inter_comp_2x2},
      {_device, inter_compound_diff_chroma, sizeof(inter_compound_diff_chroma), shader_lib->sig_inter_pred.Get(),
       &s8->inter_comp_diff_uv},
      {_device, inter_compound_diff_luma, sizeof(inter_compound_diff_luma), shader_lib->sig_inter_pred.Get(),
       &s8->inter_comp_diff_y},
      {_device, inter_compound_masked, sizeof(inter_compound_masked), shader_lib->sig_inter_pred.Get(),
       &s8->inter_comp_masked},
      {_device, inter_main, sizeof(inter_main), shader_lib->sig_inter_pred.Get(), &s8->inter_base},
      {_device, inter_obmc_above, sizeof(inter_obmc_above), shader_lib->sig_inter_pred.Get(), &s8->inter_obmc_above},
      {_device, inter_obmc_left, sizeof(inter_obmc_left), shader_lib->sig_inter_pred.Get(), &s8->inter_obmc_left},
      {_device, inter_warp, sizeof(inter_warp), shader_lib->sig_inter_pred.Get(), &s8->inter_warp},
      {_device, inter_scale_warp_compound, sizeof(inter_scale_warp_compound), shader_lib->sig_inter_pred.Get(),
       &s8->inter_warp_comp},
      {_device, intra_filter, sizeof(intra_filter), shader_lib->sig_intra_pred.Get(), &s8->intra_filter},
      {_device, intra_main, sizeof(intra_main), shader_lib->sig_intra_pred.Get(), &s8->intra_main},
      {_device, inter_ext_borders, sizeof(inter_ext_borders), shader_lib->sig_common0110.Get(), &s8->inter_ext_borders},
      {_device, reconstruct_block, sizeof(reconstruct_block), shader_lib->sig_intra_pred.Get(), &s8->reconstruct_block},
      {_device, loopfilter_vert, sizeof(loopfilter_vert), shader_lib->sig_lf.Get(), &s8->loopfilter_v},
      {_device, loopfilter_hor, sizeof(loopfilter_hor), shader_lib->sig_lf.Get(), &s8->loopfilter_h},

      {_device, inter_compound_2x2_hbd, sizeof(inter_compound_2x2_hbd), shader_lib->sig_inter_pred.Get(),
       &hbd->inter_comp_2x2},
      {_device, inter_2x2chroma_hbd, sizeof(inter_2x2chroma_hbd), shader_lib->sig_inter_pred.Get(), &hbd->inter_2x2},
      {_device, inter_compound_diff_chroma_hbd, sizeof(inter_compound_diff_chroma_hbd),
       shader_lib->sig_inter_pred.Get(), &hbd->inter_comp_diff_uv},
      {_device, inter_compound_diff_luma_hbd, sizeof(inter_compound_diff_luma_hbd), shader_lib->sig_inter_pred.Get(),
       &hbd->inter_comp_diff_y},
      {_device, inter_compound_hbd, sizeof(inter_compound_hbd), shader_lib->sig_inter_pred.Get(), &hbd->inter_comp},
      {_device, inter_compound_masked_hbd, sizeof(inter_compound_masked_hbd), shader_lib->sig_inter_pred.Get(),
       &hbd->inter_comp_masked},
      {_device, inter_main_hbd, sizeof(inter_main_hbd), shader_lib->sig_inter_pred.Get(), &hbd->inter_base},
      {_device, inter_obmc_above_hbd, sizeof(inter_obmc_above_hbd), shader_lib->sig_inter_pred.Get(),
       &hbd->inter_obmc_above},
      {_device, inter_obmc_left_hbd, sizeof(inter_obmc_left_hbd), shader_lib->sig_inter_pred.Get(),
       &hbd->inter_obmc_left},
      {_device, inter_scale_warp_compound_hbd, sizeof(inter_scale_warp_compound_hbd), shader_lib->sig_inter_pred.Get(),
       &hbd->inter_warp_comp},
      {_device, inter_warp_hbd, sizeof(inter_warp_hbd), shader_lib->sig_inter_pred.Get(), &hbd->inter_warp},
      {_device, intra_filter_hbd, sizeof(intra_filter_hbd), shader_lib->sig_intra_pred.Get(), &hbd->intra_filter},
      {_device, intra_main_hbd, sizeof(intra_main_hbd), shader_lib->sig_intra_pred.Get(), &hbd->intra_main},
      {_device, inter_ext_borders_hbd, sizeof(inter_ext_borders_hbd), shader_lib->sig_common0110.Get(),
       &hbd->inter_ext_borders},
      {_device, reconstruct_block_hbd, sizeof(reconstruct_block_hbd), shader_lib->sig_intra_pred.Get(),
       &hbd->reconstruct_block},
      {_device, loopfilter_vert_hbd, sizeof(loopfilter_vert_hbd), shader_lib->sig_lf.Get(), &hbd->loopfilter_v},
      {_device, loopfilter_hor_hbd, sizeof(loopfilter_hor_hbd), shader_lib->sig_lf.Get(), &hbd->loopfilter_h},

      {_device, inter_scale, sizeof(inter_scale), shader_lib->sig_inter_pred.Get(), &s8->inter_scale},
      {_device, inter_scale_2x2chroma, sizeof(inter_scale_2x2chroma), shader_lib->sig_inter_pred.Get(),
       &s8->inter_scale_2x2},
      {_device, inter_scale_compound, sizeof(inter_scale_compound), shader_lib->sig_inter_pred.Get(),
       &s8->inter_scale_comp},
      {_device, inter_scale_compound_diff_chroma, sizeof(inter_scale_compound_diff_chroma),
       shader_lib->sig_inter_pred.Get(), &s8->inter_scale_comp_diff_uv},
      {_device, inter_scale_compound_diff_luma, sizeof(inter_scale_compound_diff_luma),
       shader_lib->sig_inter_pred.Get(), &s8->inter_scale_comp_diff_y},
      {_device, inter_scale_compound_masked, sizeof(inter_scale_compound_masked), shader_lib->sig_inter_pred.Get(),
       &s8->inter_scale_comp_masked},
      {_device, inter_scale_obmc_above, sizeof(inter_scale_obmc_above), shader_lib->sig_inter_pred.Get(),
       &s8->inter_scale_obmc_above},
      {_device, inter_scale_obmc_left, sizeof(inter_scale_obmc_left), shader_lib->sig_inter_pred.Get(),
       &s8->inter_scale_obmc_left},
      {_device, inter_scale_hbd, sizeof(inter_scale_hbd), shader_lib->sig_inter_pred.Get(), &hbd->inter_scale},
      {_device, inter_scale_2x2chroma_hbd, sizeof(inter_scale_2x2chroma_hbd), shader_lib->sig_inter_pred.Get(),
       &hbd->inter_scale_2x2},
      {_device, inter_scale_compound_hbd, sizeof(inter_scale_compound_hbd), shader_lib->sig_inter_pred.Get(),
       &hbd->inter_scale_comp},
      {_device, inter_scale_compound_diff_chroma_hbd, sizeof(inter_scale_compound_diff_chroma_hbd),
       shader_lib->sig_inter_pred.Get(), &hbd->inter_scale_comp_diff_uv},
      {_device, inter_scale_compound_diff_luma_hbd, sizeof(inter_scale_compound_diff_luma_hbd),
       shader_lib->sig_inter_pred.Get(), &hbd->inter_scale_comp_diff_y},
      {_device, inter_scale_compound_masked_hbd, sizeof(inter_scale_compound_masked_hbd),
       shader_lib->sig_inter_pred.Get(), &hbd->inter_scale_comp_masked},
      {_device, inter_scale_obmc_above_hbd, sizeof(inter_scale_obmc_above_hbd), shader_lib->sig_inter_pred.Get(),
       &hbd->inter_scale_obmc_above},
      {_device, inter_scale_obmc_left_hbd, sizeof(inter_scale_obmc_left_hbd), shader_lib->sig_inter_pred.Get(),
       &hbd->inter_scale_obmc_left}};

  const int size = sizeof(create_tasks) / sizeof(CreateShaderTask);
  shader_lib->threads = min(threads, MAX_CREATE_SHADER_THREADS);
  assert((size + shader_lib->threads) <= BUFFERS_COUNT);

  if (!threads) {
    // sync case
    for (int i = 0; i < size; i++) {
      CreateShaderTask* task = &create_tasks[i];
      err |= create_shader(task->device, task->src, task->size, task->sig, task->dst) != S_OK;
    }
  } else {
    // async case
    shader_lib->create_shader_tasks = new CreateShaderTask[size];
    memcpy(shader_lib->create_shader_tasks, create_tasks, sizeof(create_tasks));
    MTQueueInit(&shader_lib->task_queue);
    for (int i = 0; i < size; i++) {
      MTQueuePush(&shader_lib->task_queue, &shader_lib->create_shader_tasks[i]);
    }
    for (int i = 0; i < shader_lib->threads; i++) {
      MTQueuePush(&shader_lib->task_queue, 0);
      pthread_create(&shader_lib->create_shader_threads[i], 0, (LPTHREAD_START_ROUTINE)create_shader_thread_hook,
                     shader_lib);
    }
  }
  return err ? 0 : shader_lib;
}

void av1_destroy_pipeline_cache_handle(void* handle) {
  if (handle) {
    compute_shader_lib* shader_lib = static_cast<compute_shader_lib*>(handle);
    wait_shader_create_complete(shader_lib);
    delete shader_lib;
  }
}

static int get_random_number_test(int val) {
  unsigned int bit;
  bit = ((val >> 0) ^ (val >> 1) ^ (val >> 3) ^ (val >> 12)) & 1;
  val = (val >> 1) | (bit << 15);
  return val;
}

HRESULT av1_upload_luts(Av1Core* dec) {
  ComPtr<ID3D12Resource> tmp_fg_luma;
  ComPtr<ID3D12Resource> tmp_fg_chroma;
  ComPtr<ID3D12Resource> tmp_fg_gaus;
  ComPtr<ID3D12Resource> tmp_pred_wedge;
  ComPtr<ID3D12Resource> tmp_warp_filt;

  GpuBufferObject* fg_luma = dec->filmgrain_random_luma;
  GpuBufferObject* fg_chroma = dec->filmgrain_random_chroma;
  GpuBufferObject* fg_gaus = dec->filmgrain_gaus;
  GpuBufferObject* pred_lut = dec->inter_mask_lut;
  GpuBufferObject* warp = dec->inter_warp_filter;

  HRESULT hr;
  Microsoft::WRL::ComPtr<ID3D12Device> device = dec->compute.device;
  hr = device->CreateCommittedResource(&CD3DX12_HEAP_PROPERTIES(D3D12_HEAP_TYPE_UPLOAD), D3D12_HEAP_FLAG_NONE,
                                       &CD3DX12_RESOURCE_DESC::Buffer(fg_luma->size), D3D12_RESOURCE_STATE_GENERIC_READ,
                                       NULL, IID_PPV_ARGS(&tmp_fg_luma));
  hr |= device->CreateCommittedResource(&CD3DX12_HEAP_PROPERTIES(D3D12_HEAP_TYPE_UPLOAD), D3D12_HEAP_FLAG_NONE,
                                        &CD3DX12_RESOURCE_DESC::Buffer(fg_chroma->size),
                                        D3D12_RESOURCE_STATE_GENERIC_READ, NULL, IID_PPV_ARGS(&tmp_fg_chroma));
  hr |= device->CreateCommittedResource(&CD3DX12_HEAP_PROPERTIES(D3D12_HEAP_TYPE_UPLOAD), D3D12_HEAP_FLAG_NONE,
                                        &CD3DX12_RESOURCE_DESC::Buffer(fg_gaus->size),
                                        D3D12_RESOURCE_STATE_GENERIC_READ, NULL, IID_PPV_ARGS(&tmp_fg_gaus));
  hr |= device->CreateCommittedResource(&CD3DX12_HEAP_PROPERTIES(D3D12_HEAP_TYPE_UPLOAD), D3D12_HEAP_FLAG_NONE,
                                        &CD3DX12_RESOURCE_DESC::Buffer(pred_lut->size),
                                        D3D12_RESOURCE_STATE_GENERIC_READ, NULL, IID_PPV_ARGS(&tmp_pred_wedge));
  hr |= device->CreateCommittedResource(&CD3DX12_HEAP_PROPERTIES(D3D12_HEAP_TYPE_UPLOAD), D3D12_HEAP_FLAG_NONE,
                                        &CD3DX12_RESOURCE_DESC::Buffer(warp->size), D3D12_RESOURCE_STATE_GENERIC_READ,
                                        NULL, IID_PPV_ARGS(&tmp_warp_filt));
  if (FAILED(hr)) return hr;

  int* dst = NULL;
  hr = tmp_fg_luma->Map(0, &CD3DX12_RANGE(0, 0), (void**)&dst);
  if (FAILED(hr)) return hr;
  for (int i = 0; i < 65536; i++) {
    int random_register = i;
    for (int j = 0; j < 82; j++) {
      random_register = get_random_number_test(random_register);
    }
    dst[i] = random_register;
  }
  tmp_fg_luma->Unmap(0, NULL);

  hr = tmp_fg_chroma->Map(0, &CD3DX12_RANGE(0, 0), (void**)&dst);
  if (FAILED(hr)) return hr;
  for (int i = 0; i < 65536; i++) {
    int random_register = i;
    for (int j = 0; j < 44; j++) {
      random_register = get_random_number_test(random_register);
    }
    dst[i] = random_register;
  }
  tmp_fg_chroma->Unmap(0, NULL);

  hr = tmp_fg_gaus->Map(0, &CD3DX12_RANGE(0, 0), (void**)&dst);
  if (FAILED(hr)) return hr;
  memcpy(dst, dx_gaussian_sequence, sizeof(dx_gaussian_sequence));
  tmp_fg_gaus->Unmap(0, NULL);

  hr = tmp_warp_filt->Map(0, &CD3DX12_RANGE(0, 0), (void**)&dst);
  if (FAILED(hr)) return hr;

  const int16_t* wrp_src = &warped_filter[0][0];
  for (int i = 0; i < sizeof(warped_filter) / 2; ++i) {
    dst[i] = wrp_src[i];
  }
  tmp_warp_filt->Unmap(0, NULL);

  uint8_t* ptr = NULL;
  hr = tmp_pred_wedge->Map(0, &CD3DX12_RANGE(0, 0), (void**)&ptr);
  if (FAILED(hr)) return hr;
  int offset = 0;
  av1_init_wedge_masks();
  for (int bsize = 0; bsize < BLOCK_SIZES_ALL; ++bsize) {
    const wedge_params_type* wedge_params = &wedge_params_lookup[bsize];
    if (!wedge_params->bits) continue;
    const int bw = block_size_wide[bsize];
    const int bh = block_size_high[bsize];
    const int size = bw * bh;
    dec->wedge_offsets[bsize][0] = (offset + 63) >> 6;
    for (int index = 0; index < 16; ++index) {
      memcpy(ptr + offset, wedge_params->masks[0][index], size);
      offset += size;
      memcpy(ptr + offset, wedge_params->masks[1][index], size);
      offset += size;
    }

    const int size_scale = ii_size_scales[bsize];

    for (int i = 0; i < bh; ++i) memset(ptr + offset + i * bw, 32, bw);
    offset += size;
    for (int i = 0; i < bh; ++i) memset(ptr + offset + i * bw, ii_weights1d[i * size_scale], bw);
    offset += size;
    for (int y = 0; y < bh; ++y)
      for (int x = 0; x < bw; ++x) ptr[offset + x + y * bw] = ii_weights1d[x * size_scale];
    offset += size;
    for (int y = 0; y < bh; ++y)
      for (int x = 0; x < bw; ++x) ptr[offset + x + y * bw] = ii_weights1d[(y < x ? y : x) * size_scale];
    offset += size;
  }

  for (int bsize = 0; bsize < BLOCK_SIZES_ALL; ++bsize) {
    const wedge_params_type* wedge_params = &wedge_params_lookup[bsize];
    if (!wedge_params->bits) continue;
    const int bw = block_size_wide[bsize] >> 1;
    const int bh = block_size_high[bsize] >> 1;
    const int size = AOMMAX(64, bw * bh);
    dec->wedge_offsets[bsize][1] = (offset + 63) >> 6;
    for (int index = 0; index < 16; ++index) {
      for (int sign = 0; sign < 2; ++sign) {
        int idx = 0;
        for (int y = 0; y < bh; ++y) {
          for (int x = 0; x < bw; ++x) {
            ptr[offset + idx] = (wedge_params->masks[sign][index][x * 2 + (y * 2 + 0) * bw * 2] +
                                 wedge_params->masks[sign][index][x * 2 + 1 + (y * 2 + 0) * bw * 2] +
                                 wedge_params->masks[sign][index][x * 2 + (y * 2 + 1) * bw * 2] +
                                 wedge_params->masks[sign][index][x * 2 + 1 + (y * 2 + 1) * bw * 2] + 2) >>
                                2;
            ++idx;
          }
        }
        offset += size;
      }
    }
    const int plane_bsize = ss_size_lookup[bsize][1][1];
    const int size_scale = ii_size_scales[plane_bsize];
    for (int i = 0; i < bh; ++i) memset(ptr + offset + i * bw, 32, bw);
    offset += size;
    for (int i = 0; i < bh; ++i) memset(ptr + offset + i * bw, ii_weights1d[i * size_scale], bw);
    offset += size;
    for (int y = 0; y < bh; ++y)
      for (int x = 0; x < bw; ++x) ptr[offset + x + y * bw] = ii_weights1d[x * size_scale];
    offset += size;
    for (int y = 0; y < bh; ++y)
      for (int x = 0; x < bw; ++x) ptr[offset + x + y * bw] = ii_weights1d[(y < x ? y : x) * size_scale];
    offset += size;
  }
  tmp_pred_wedge->Unmap(0, NULL);

  ComPtr<ID3D12CommandAllocator> computeAllocator;
  hr = device->CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT, IID_PPV_ARGS(&computeAllocator));
  if (FAILED(hr)) return hr;

  ComPtr<ID3D12GraphicsCommandList> clist;
  hr = device->CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_DIRECT, computeAllocator.Get(), NULL, IID_PPV_ARGS(&clist));
  if (FAILED(hr)) return hr;

  clist->CopyResource(fg_luma->dev, tmp_fg_luma.Get());
  clist->CopyResource(fg_chroma->dev, tmp_fg_chroma.Get());
  clist->CopyResource(fg_gaus->dev, tmp_fg_gaus.Get());
  clist->CopyResource(pred_lut->dev, tmp_pred_wedge.Get());
  clist->CopyResource(warp->dev, tmp_warp_filt.Get());

  D3D12_RESOURCE_BARRIER barriers[] = {
      CD3DX12_RESOURCE_BARRIER::Transition(fg_luma->dev, D3D12_RESOURCE_STATE_COPY_DEST,
                                           D3D12_RESOURCE_STATE_GENERIC_READ),
      CD3DX12_RESOURCE_BARRIER::Transition(fg_chroma->dev, D3D12_RESOURCE_STATE_COPY_DEST,
                                           D3D12_RESOURCE_STATE_GENERIC_READ),
      CD3DX12_RESOURCE_BARRIER::Transition(fg_gaus->dev, D3D12_RESOURCE_STATE_COPY_DEST,
                                           D3D12_RESOURCE_STATE_GENERIC_READ),
      CD3DX12_RESOURCE_BARRIER::Transition(pred_lut->dev, D3D12_RESOURCE_STATE_COPY_DEST,
                                           D3D12_RESOURCE_STATE_GENERIC_READ),
      CD3DX12_RESOURCE_BARRIER::Transition(warp->dev, D3D12_RESOURCE_STATE_COPY_DEST,
                                           D3D12_RESOURCE_STATE_GENERIC_READ),
  };

  clist->ResourceBarrier(5, barriers);
  clist->Close();

  ID3D12CommandList* cl[] = {clist.Get()};
  Microsoft::WRL::ComPtr<ID3D12CommandQueue> queue = dec->compute.queue_direct;
  queue->ExecuteCommandLists(1, cl);

  ComPtr<ID3D12Fence> fence;
  hr = device->CreateFence(0, D3D12_FENCE_FLAG_NONE, IID_PPV_ARGS(&fence));
  if (FAILED(hr)) return hr;

  queue->Signal(fence.Get(), 1);
  HANDLE event = CreateEvent(nullptr, false, false, nullptr);
  fence->SetEventOnCompletion(1, event);
  WaitForSingleObject(event, INFINITE);
  return hr;
}

static const int dx_gaussian_sequence[2048] = {
    56,    568,   -180,  172,   124,   -84,   172,   -64,   -900,  24,    820,   224,   1248,  996,   272,   -8,
    -916,  -388,  -732,  -104,  -188,  800,   112,   -652,  -320,  -376,  140,   -252,  492,   -168,  44,    -788,
    588,   -584,  500,   -228,  12,    680,   272,   -476,  972,   -100,  652,   368,   432,   -196,  -720,  -192,
    1000,  -332,  652,   -136,  -552,  -604,  -4,    192,   -220,  -136,  1000,  -52,   372,   -96,   -624,  124,
    -24,   396,   540,   -12,   -104,  640,   464,   244,   -208,  -84,   368,   -528,  -740,  248,   -968,  -848,
    608,   376,   -60,   -292,  -40,   -156,  252,   -292,  248,   224,   -280,  400,   -244,  244,   -60,   76,
    -80,   212,   532,   340,   128,   -36,   824,   -352,  -60,   -264,  -96,   -612,  416,   -704,  220,   -204,
    640,   -160,  1220,  -408,  900,   336,   20,    -336,  -96,   -792,  304,   48,    -28,   -1232, -1172, -448,
    104,   -292,  -520,  244,   60,    -948,  0,     -708,  268,   108,   356,   -548,  488,   -344,  -136,  488,
    -196,  -224,  656,   -236,  -1128, 60,    4,     140,   276,   -676,  -376,  168,   -108,  464,   8,     564,
    64,    240,   308,   -300,  -400,  -456,  -136,  56,    120,   -408,  -116,  436,   504,   -232,  328,   844,
    -164,  -84,   784,   -168,  232,   -224,  348,   -376,  128,   568,   96,    -1244, -288,  276,   848,   832,
    -360,  656,   464,   -384,  -332,  -356,  728,   -388,  160,   -192,  468,   296,   224,   140,   -776,  -100,
    280,   4,     196,   44,    -36,   -648,  932,   16,    1428,  28,    528,   808,   772,   20,    268,   88,
    -332,  -284,  124,   -384,  -448,  208,   -228,  -1044, -328,  660,   380,   -148,  -300,  588,   240,   540,
    28,    136,   -88,   -436,  256,   296,   -1000, 1400,  0,     -48,   1056,  -136,  264,   -528,  -1108, 632,
    -484,  -592,  -344,  796,   124,   -668,  -768,  388,   1296,  -232,  -188,  -200,  -288,  -4,    308,   100,
    -168,  256,   -500,  204,   -508,  648,   -136,  372,   -272,  -120,  -1004, -552,  -548,  -384,  548,   -296,
    428,   -108,  -8,    -912,  -324,  -224,  -88,   -112,  -220,  -100,  996,   -796,  548,   360,   -216,  180,
    428,   -200,  -212,  148,   96,    148,   284,   216,   -412,  -320,  120,   -300,  -384,  -604,  -572,  -332,
    -8,    -180,  -176,  696,   116,   -88,   628,   76,    44,    -516,  240,   -208,  -40,   100,   -592,  344,
    -308,  -452,  -228,  20,    916,   -1752, -136,  -340,  -804,  140,   40,    512,   340,   248,   184,   -492,
    896,   -156,  932,   -628,  328,   -688,  -448,  -616,  -752,  -100,  560,   -1020, 180,   -800,  -64,   76,
    576,   1068,  396,   660,   552,   -108,  -28,   320,   -628,  312,   -92,   -92,   -472,  268,   16,    560,
    516,   -672,  -52,   492,   -100,  260,   384,   284,   292,   304,   -148,  88,    -152,  1012,  1064,  -228,
    164,   -376,  -684,  592,   -392,  156,   196,   -524,  -64,   -884,  160,   -176,  636,   648,   404,   -396,
    -436,  864,   424,   -728,  988,   -604,  904,   -592,  296,   -224,  536,   -176,  -920,  436,   -48,   1176,
    -884,  416,   -776,  -824,  -884,  524,   -548,  -564,  -68,   -164,  -96,   692,   364,   -692,  -1012, -68,
    260,   -480,  876,   -1116, 452,   -332,  -352,  892,   -1088, 1220,  -676,  12,    -292,  244,   496,   372,
    -32,   280,   200,   112,   -440,  -96,   24,    -644,  -184,  56,    -432,  224,   -980,  272,   -260,  144,
    -436,  420,   356,   364,   -528,  76,    172,   -744,  -368,  404,   -752,  -416,  684,   -688,  72,    540,
    416,   92,    444,   480,   -72,   -1416, 164,   -1172, -68,   24,    424,   264,   1040,  128,   -912,  -524,
    -356,  64,    876,   -12,   4,     -88,   532,   272,   -524,  320,   276,   -508,  940,   24,    -400,  -120,
    756,   60,    236,   -412,  100,   376,   -484,  400,   -100,  -740,  -108,  -260,  328,   -268,  224,   -200,
    -416,  184,   -604,  -564,  -20,   296,   60,    892,   -888,  60,    164,   68,    -760,  216,   -296,  904,
    -336,  -28,   404,   -356,  -568,  -208,  -1480, -512,  296,   328,   -360,  -164,  -1560, -776,  1156,  -428,
    164,   -504,  -112,  120,   -216,  -148,  -264,  308,   32,    64,    -72,   72,    116,   176,   -64,   -272,
    460,   -536,  -784,  -280,  348,   108,   -752,  -132,  524,   -540,  -776,  116,   -296,  -1196, -288,  -560,
    1040,  -472,  116,   -848,  -1116, 116,   636,   696,   284,   -176,  1016,  204,   -864,  -648,  -248,  356,
    972,   -584,  -204,  264,   880,   528,   -24,   -184,  116,   448,   -144,  828,   524,   212,   -212,  52,
    12,    200,   268,   -488,  -404,  -880,  824,   -672,  -40,   908,   -248,  500,   716,   -576,  492,   -576,
    16,    720,   -108,  384,   124,   344,   280,   576,   -500,  252,   104,   -308,  196,   -188,  -8,    1268,
    296,   1032,  -1196, 436,   316,   372,   -432,  -200,  -660,  704,   -224,  596,   -132,  268,   32,    -452,
    884,   104,   -1008, 424,   -1348, -280,  4,     -1168, 368,   476,   696,   300,   -8,    24,    180,   -592,
    -196,  388,   304,   500,   724,   -160,  244,   -84,   272,   -256,  -420,  320,   208,   -144,  -156,  156,
    364,   452,   28,    540,   316,   220,   -644,  -248,  464,   72,    360,   32,    -388,  496,   -680,  -48,
    208,   -116,  -408,  60,    -604,  -392,  548,   -840,  784,   -460,  656,   -544,  -388,  -264,  908,   -800,
    -628,  -612,  -568,  572,   -220,  164,   288,   -16,   -308,  308,   -112,  -636,  -760,  280,   -668,  432,
    364,   240,   -196,  604,   340,   384,   196,   592,   -44,   -500,  432,   -580,  -132,  636,   -76,   392,
    4,     -412,  540,   508,   328,   -356,  -36,   16,    -220,  -64,   -248,  -60,   24,    -192,  368,   1040,
    92,    -24,   -1044, -32,   40,    104,   148,   192,   -136,  -520,  56,    -816,  -224,  732,   392,   356,
    212,   -80,   -424,  -1008, -324,  588,   -1496, 576,   460,   -816,  -848,  56,    -580,  -92,   -1372, -112,
    -496,  200,   364,   52,    -140,  48,    -48,   -60,   84,    72,    40,    132,   -356,  -268,  -104,  -284,
    -404,  732,   -520,  164,   -304,  -540,  120,   328,   -76,   -460,  756,   388,   588,   236,   -436,  -72,
    -176,  -404,  -316,  -148,  716,   -604,  404,   -72,   -88,   -888,  -68,   944,   88,    -220,  -344,  960,
    472,   460,   -232,  704,   120,   832,   -228,  692,   -508,  132,   -476,  844,   -748,  -364,  -44,   1116,
    -1104, -1056, 76,    428,   552,   -692,  60,    356,   96,    -384,  -188,  -612,  -576,  736,   508,   892,
    352,   -1132, 504,   -24,   -352,  324,   332,   -600,  -312,  292,   508,   -144,  -8,    484,   48,    284,
    -260,  -240,  256,   -100,  -292,  -204,  -44,   472,   -204,  908,   -188,  -1000, -256,  92,    1164,  -392,
    564,   356,   652,   -28,   -884,  256,   484,   -192,  760,   -176,  376,   -524,  -452,  -436,  860,   -736,
    212,   124,   504,   -476,  468,   76,    -472,  552,   -692,  -944,  -620,  740,   -240,  400,   132,   20,
    192,   -196,  264,   -668,  -1012, -60,   296,   -316,  -828,  76,    -156,  284,   -768,  -448,  -832,  148,
    248,   652,   616,   1236,  288,   -328,  -400,  -124,  588,   220,   520,   -696,  1032,  768,   -740,  -92,
    -272,  296,   448,   -464,  412,   -200,  392,   440,   -200,  264,   -152,  -260,  320,   1032,  216,   320,
    -8,    -64,   156,   -1016, 1084,  1172,  536,   484,   -432,  132,   372,   -52,   -256,  84,    116,   -352,
    48,    116,   304,   -384,  412,   924,   -300,  528,   628,   180,   648,   44,    -980,  -220,  1320,  48,
    332,   748,   524,   -268,  -720,  540,   -276,  564,   -344,  -208,  -196,  436,   896,   88,    -392,  132,
    80,    -964,  -288,  568,   56,    -48,   -456,  888,   8,     552,   -156,  -292,  948,   288,   128,   -716,
    -292,  1192,  -152,  876,   352,   -600,  -260,  -812,  -468,  -28,   -120,  -32,   -44,   1284,  496,   192,
    464,   312,   -76,   -516,  -380,  -456,  -1012, -48,   308,   -156,  36,    492,   -156,  -808,  188,   1652,
    68,    -120,  -116,  316,   160,   -140,  352,   808,   -416,  592,   316,   -480,  56,    528,   -204,  -568,
    372,   -232,  752,   -344,  744,   -4,    324,   -416,  -600,  768,   268,   -248,  -88,   -132,  -420,  -432,
    80,    -288,  404,   -316,  -1216, -588,  520,   -108,  92,    -320,  368,   -480,  -216,  -92,   1688,  -300,
    180,   1020,  -176,  820,   -68,   -228,  -260,  436,   -904,  20,    40,    -508,  440,   -736,  312,   332,
    204,   760,   -372,  728,   96,    -20,   -632,  -520,  -560,  336,   1076,  -64,   -532,  776,   584,   192,
    396,   -728,  -520,  276,   -188,  80,    -52,   -612,  -252,  -48,   648,   212,   -688,  228,   -52,   -260,
    428,   -412,  -272,  -404,  180,   816,   -796,  48,    152,   484,   -88,   -216,  988,   696,   188,   -528,
    648,   -116,  -180,  316,   476,   12,    -564,  96,    476,   -252,  -364,  -376,  -392,  556,   -256,  -576,
    260,   -352,  120,   -16,   -136,  -260,  -492,  72,    556,   660,   580,   616,   772,   436,   424,   -32,
    -324,  -1268, 416,   -324,  -80,   920,   160,   228,   724,   32,    -516,  64,    384,   68,    -128,  136,
    240,   248,   -204,  -68,   252,   -932,  -120,  -480,  -628,  -84,   192,   852,   -404,  -288,  -132,  204,
    100,   168,   -68,   -196,  -868,  460,   1080,  380,   -80,   244,   0,     484,   -888,  64,    184,   352,
    600,   460,   164,   604,   -196,  320,   -64,   588,   -184,  228,   12,    372,   48,    -848,  -344,  224,
    208,   -200,  484,   128,   -20,   272,   -468,  -840,  384,   256,   -720,  -520,  -464,  -580,  112,   -120,
    644,   -356,  -208,  -608,  -528,  704,   560,   -424,  392,   828,   40,    84,    200,   -152,  0,     -144,
    584,   280,   -120,  80,    -556,  -972,  -196,  -472,  724,   80,    168,   -32,   88,    160,   -688,  0,
    160,   356,   372,   -776,  740,   -128,  676,   -248,  -480,  4,     -364,  96,    544,   232,   -1032, 956,
    236,   356,   20,    -40,   300,   24,    -676,  -596,  132,   1120,  -104,  532,   -1096, 568,   648,   444,
    508,   380,   188,   -376,  -604,  1488,  424,   24,    756,   -220,  -192,  716,   120,   920,   688,   168,
    44,    -460,  568,   284,   1144,  1160,  600,   424,   888,   656,   -356,  -320,  220,   316,   -176,  -724,
    -188,  -816,  -628,  -348,  -228,  -380,  1012,  -452,  -660,  736,   928,   404,   -696,  -72,   -268,  -892,
    128,   184,   -344,  -780,  360,   336,   400,   344,   428,   548,   -112,  136,   -228,  -216,  -820,  -516,
    340,   92,    -136,  116,   -300,  376,   -244,  100,   -316,  -520,  -284,  -12,   824,   164,   -548,  -180,
    -128,  116,   -924,  -828,  268,   -368,  -580,  620,   192,   160,   0,     -1676, 1068,  424,   -56,   -360,
    468,   -156,  720,   288,   -528,  556,   -364,  548,   -148,  504,   316,   152,   -648,  -620,  -684,  -24,
    -376,  -384,  -108,  -920,  -1032, 768,   180,   -264,  -508,  -1268, -260,  -60,   300,   -240,  988,   724,
    -376,  -576,  -212,  -736,  556,   192,   1092,  -620,  -880,  376,   -56,   -4,    -216,  -32,   836,   268,
    396,   1332,  864,   -600,  100,   56,    -412,  -92,   356,   180,   884,   -468,  -436,  292,   -388,  -804,
    -704,  -840,  368,   -348,  140,   -724,  1536,  940,   372,   112,   -372,  436,   -480,  1136,  296,   -32,
    -228,  132,   -48,   -220,  868,   -1016, -60,   -1044, -464,  328,   916,   244,   12,    -736,  -296,  360,
    468,   -376,  -108,  -92,   788,   368,   -56,   544,   400,   -672,  -420,  728,   16,    320,   44,    -284,
    -380,  -796,  488,   132,   204,   -596,  -372,  88,    -152,  -908,  -636,  -572,  -624,  -116,  -692,  -200,
    -56,   276,   -88,   484,   -324,  948,   864,   1000,  -456,  -184,  -276,  292,   -296,  156,   676,   320,
    160,   908,   -84,   -1236, -288,  -116,  260,   -372,  -644,  732,   -756,  -96,   84,    344,   -520,  348,
    -688,  240,   -84,   216,   -1044, -136,  -676,  -396,  -1500, 960,   -40,   176,   168,   1516,  420,   -504,
    -344,  -364,  -360,  1216,  -940,  -380,  -212,  252,   -660,  -708,  484,   -444,  -152,  928,   -120,  1112,
    476,   -260,  560,   -148,  -344,  108,   -196,  228,   -288,  504,   560,   -328,  -88,   288,   -1008, 460,
    -228,  468,   -836,  -196,  76,    388,   232,   412,   -1168, -716,  -644,  756,   -172,  -356,  -504,  116,
    432,   528,   48,    476,   -168,  -608,  448,   160,   -532,  -272,  28,    -676,  -12,   828,   980,   456,
    520,   104,   -104,  256,   -344,  -4,    -28,   -368,  -52,   -524,  -572,  -556,  -200,  768,   1124,  -208,
    -512,  176,   232,   248,   -148,  -888,  604,   -600,  -304,  804,   -156,  -212,  488,   -192,  -804,  -256,
    368,   -360,  -916,  -328,  228,   -240,  -448,  -472,  856,   -556,  -364,  572,   -12,   -156,  -368,  -340,
    432,   252,   -752,  -152,  288,   268,   -580,  -848,  -592,  108,   -76,   244,   312,   -716,  592,   -80,
    436,   360,   4,     -248,  160,   516,   584,   732,   44,    -468,  -280,  -292,  -156,  -588,  28,    308,
    912,   24,    124,   156,   180,   -252,  944,   -924,  -772,  -520,  -428,  -624,  300,   -212,  -1144, 32,
    -724,  800,   -1128, -212,  -1288, -848,  180,   -416,  440,   192,   -576,  -792,  -76,   -1080, 80,    -532,
    -352,  -132,  380,   -820,  148,   1112,  128,   164,   456,   700,   -924,  144,   -668,  -384,  648,   -832,
    508,   552,   -52,   -100,  -656,  208,   -568,  748,   -88,   680,   232,   300,   192,   -408,  -1012, -152,
    -252,  -268,  272,   -876,  -664,  -648,  -332,  -136,  16,    12,    1152,  -28,   332,   -536,  320,   -672,
    -460,  -316,  532,   -260,  228,   -40,   1052,  -816,  180,   88,    -496,  -556,  -672,  -368,  428,   92,
    356,   404,   -408,  252,   196,   -176,  -556,  792,   268,   32,    372,   40,    96,    -332,  328,   120,
    372,   -900,  -40,   472,   -264,  -592,  952,   128,   656,   112,   664,   -232,  420,   4,     -344,  -464,
    556,   244,   -416,  -32,   252,   0,     -412,  188,   -696,  508,   -476,  324,   -1096, 656,   -312,  560,
    264,   -136,  304,   160,   -64,   -580,  248,   336,   -720,  560,   -348,  -288,  -276,  -196,  -500,  852,
    -544,  -236,  -1128, -992,  -776,  116,   56,    52,    860,   884,   212,   -12,   168,   1020,  512,   -552,
    924,   -148,  716,   188,   164,   -340,  -520,  -184,  880,   -152,  -680,  -208,  -1156, -300,  -528,  -472,
    364,   100,   -744,  -1056, -32,   540,   280,   144,   -676,  -32,   -232,  -280,  -224,  96,    568,   -76,
    172,   148,   148,   104,   32,    -296,  -32,   788,   -80,   32,    -16,   280,   288,   944,   428,   -484};
