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

#include "av1/common/restoration.h"
#include "av1/common/av1_loopfilter.h"

struct LoopfilterData {
  int planes[3][4];
  int limits[64][4];
};

struct LoopfilterSRT {
  int wicount;
  int plane;
  int offset_base;
  int block_cols;
  int block_id_offset;
};

struct GenLfData {
  int mi_stride;
  int mi_addr_base;
  int delta_q_info_delta_lf_present_flag;
  int delta_q_info_delta_lf_multi;
  int lf_mode_ref_delta_enabled;
  int r[3];

  int lf_filter_level[2];
  int lf_filter_level_u;
  int lf_filter_level_v;

  int lf_mode_deltas[2][4];
  int lf_ref_deltas[8][4];
  int seg_features[8][4];
  int seg_data[8][8][4];
  int lfi_n_lvl[3][8][8][2][4];
};

struct GenLfSRT {
  int wicount;
  int mi_cols;
  int mi_rows;
  int plane;
  int dst_offset;
  int dst_stride;
};

void av1_loopfilter_gpu(Av1Core *dec, AV1_COMMON *cm, MACROBLOCKD *xd) {
  if (!(cm->lf.filter_level[0] || cm->lf.filter_level[1])) {
    return;
  }
  av1_loop_filter_frame_init(cm, 0, 3);

  av1_frame_thread_data *td = dec->curr_frame_data;
  HwFrameBuffer *dst = td->dst_frame_buffer;
  ComputeCommandBuffer *cb = &td->command_buffer;
  Microsoft::WRL::ComPtr<ID3D12GraphicsCommandList> command_list = dec->compute.command_list;
  ComputeShader *shader;

  const int mi_cols_y = AOMMIN(cm->mi_cols, (dst->y_crop_width + 3) >> 2);
  const int mi_rows_y = AOMMIN(cm->mi_rows, (dst->y_crop_height + 3) >> 2);
  const int mi_cols_uv = AOMMIN((cm->mi_cols + 1) >> 1, (dst->uv_crop_width + 3) >> 2);
  const int mi_rows_uv = AOMMIN((cm->mi_rows + 1) >> 1, (dst->uv_crop_height + 3) >> 2);

  const int blk_cols[] = {
      (mi_cols_y + 15) >> 4, (mi_cols_uv + 15) >> 4, (mi_cols_uv + 15) >> 4, mi_cols_y, mi_cols_uv, mi_cols_uv};

  int blk_count[] = {
      blk_cols[0] * mi_rows_y,
      blk_cols[1] * mi_rows_uv,
      blk_cols[2] * mi_rows_uv,
      blk_cols[3] * ((mi_rows_y + 15) >> 4),
      blk_cols[4] * ((mi_rows_uv + 15) >> 4),
      blk_cols[5] * ((mi_rows_uv + 15) >> 4),
  };

  int blk_offsets[6];
  blk_offsets[0] = 0;
  for (int i = 1; i < 6; ++i) blk_offsets[i] = blk_offsets[i - 1] + blk_count[i - 1];

  ConstantBufferObject cbo = cb->Alloc(sizeof(GenLfData));
  GenLfData *gen_data = (GenLfData *)cbo.host_ptr;
  gen_data->mi_stride = cm->mi_stride;
  gen_data->mi_addr_base = static_cast<int>((uint64_t)dec->mode_info_pool->host_ptr);
  gen_data->delta_q_info_delta_lf_multi = cm->delta_q_info.delta_lf_multi;
  gen_data->delta_q_info_delta_lf_present_flag = cm->delta_q_info.delta_lf_present_flag;
  gen_data->lf_mode_ref_delta_enabled = cm->lf.mode_ref_delta_enabled;
  gen_data->lf_filter_level[0] = cm->lf.filter_level[0];
  gen_data->lf_filter_level[1] = cm->lf.filter_level[1];
  gen_data->lf_filter_level_u = cm->lf.filter_level_u;
  gen_data->lf_filter_level_v = cm->lf.filter_level_v;
  gen_data->lf_mode_deltas[0][0] = cm->lf.mode_deltas[0];
  gen_data->lf_mode_deltas[1][0] = cm->lf.mode_deltas[1];
  for (int i = 0; i < 8; ++i) gen_data->lf_ref_deltas[i][0] = cm->lf.ref_deltas[i];
  for (int i = 0; i < 8; ++i) {
    gen_data->seg_features[i][0] = xd->lossless[i];
    gen_data->seg_features[i][1] = cm->seg.enabled ? cm->seg.feature_mask[i] : 0;
    for (int j = 0; j < 8; ++j) gen_data->seg_data[i][j][0] = cm->seg.feature_data[i][j];
  }
  for (int p = 0; p < 3; ++p)
    for (int s = 0; s < 8; ++s)
      for (int r = 0; r < 8; ++r)
        for (int m = 0; m < 2; ++m) {
          gen_data->lfi_n_lvl[p][s][r][m][0] = cm->lf_info.lvl[p][s][0][r][m];
          gen_data->lfi_n_lvl[p][s][r][m][1] = cm->lf_info.lvl[p][s][1][r][m];
        }

  GenLfSRT gen_srt;
  shader = &dec->shader_lib->shader_gen_lf_vert;

  command_list->SetComputeRootSignature(shader->signaturePtr.Get());
  command_list->SetComputeRootShaderResourceView(0, dec->mode_info_pool->dev->GetGPUVirtualAddress());
  command_list->SetComputeRootShaderResourceView(1, td->mode_info_grid->dev->GetGPUVirtualAddress());
  command_list->SetComputeRootUnorderedAccessView(2, dec->loopfilter_blocks->dev->GetGPUVirtualAddress());
  command_list->SetComputeRootConstantBufferView(3, cbo.dev_address);

  command_list->SetPipelineState(shader->pso.Get());
  for (int plane = 0; plane < 3; ++plane) {
    if ((plane == 1 && !cm->lf.filter_level_u) || (plane == 2 && !cm->lf.filter_level_v)) continue;

    const int mi_cols = plane ? mi_cols_uv : mi_cols_y;
    const int mi_rows = plane ? mi_rows_uv : mi_rows_y;
    gen_srt.plane = plane;
    gen_srt.mi_cols = mi_cols;
    gen_srt.mi_rows = mi_rows;
    gen_srt.dst_offset = blk_offsets[plane] * 32;
    gen_srt.dst_stride = blk_cols[plane] * 32;
    gen_srt.wicount = mi_rows * ((mi_cols + 15) >> 4);
    command_list->SetComputeRoot32BitConstants(4, 6, &gen_srt, 0);
    command_list->Dispatch((gen_srt.wicount + 63) >> 6, 1, 1);
  }
  shader = &dec->shader_lib->shader_gen_lf_hor;
  command_list->SetPipelineState(shader->pso.Get());
  for (int plane = 0; plane < 3; ++plane) {
    if ((plane == 1 && !cm->lf.filter_level_u) || (plane == 2 && !cm->lf.filter_level_v)) continue;

    const int mi_cols = plane ? mi_cols_uv : mi_cols_y;
    const int mi_rows = plane ? mi_rows_uv : mi_rows_y;
    gen_srt.plane = plane;
    gen_srt.mi_cols = mi_cols;
    gen_srt.mi_rows = mi_rows;
    gen_srt.dst_offset = blk_offsets[plane + 3] * 32;
    gen_srt.dst_stride = blk_cols[plane + 3] * 2;
    gen_srt.wicount = mi_cols * ((mi_rows + 15) >> 4);
    command_list->SetComputeRoot32BitConstants(4, 6, &gen_srt, 0);
    command_list->Dispatch((gen_srt.wicount + 63) >> 6, 1, 1);
  }
  command_list->ResourceBarrier(1, &CD3DX12_RESOURCE_BARRIER::UAV(dec->loopfilter_blocks->dev));
  av1_perf_gpu_mark(dec, AV1_PERF_GPU_LF_GEN);

  cbo = cb->Alloc(sizeof(LoopfilterData));
  LoopfilterData *data = (LoopfilterData *)cbo.host_ptr;
  memcpy(data->planes, td->frame_buffer->planes, sizeof(data->planes));
  for (int lvl = 0; lvl < 64; ++lvl) {
    data->limits[lvl][0] = cm->lf_info.lfthr[lvl].lim[0];
    data->limits[lvl][1] = cm->lf_info.lfthr[lvl].mblim[0];
    data->limits[lvl][2] = cm->lf_info.lfthr[lvl].hev_thr[0];
  }

  LoopfilterSRT srt;
  shader = &td->shaders->loopfilter_v;
  command_list->SetComputeRootSignature(shader->signaturePtr.Get());
  command_list->SetComputeRootShaderResourceView(0, dec->loopfilter_blocks->dev->GetGPUVirtualAddress());
  command_list->SetComputeRootUnorderedAccessView(1, dec->frame_buffer_pool->dev->GetGPUVirtualAddress());
  command_list->SetComputeRootConstantBufferView(2, cbo.dev_address);
  command_list->SetPipelineState(shader->pso.Get());
  srt.block_id_offset = 0;
  for (int p = 0; p < 3; ++p) {
    if (p == 1 && !cm->lf.filter_level_u) continue;
    if (p == 2 && !cm->lf.filter_level_v) continue;
    srt.plane = p;
    srt.offset_base = blk_offsets[p];
    srt.block_cols = blk_cols[p];
    srt.wicount = ((blk_count[p] + 1) >> 1) * 4;
    command_list->SetComputeRoot32BitConstants(3, 5, &srt, 0);
    command_list->Dispatch((srt.wicount + 63) >> 6, 1, 1);
  }
  command_list->ResourceBarrier(1, &CD3DX12_RESOURCE_BARRIER::UAV(dec->frame_buffer_pool->dev));
  av1_stage_dump_fb(dec, "lf_vert_even", td->frame_buffer);

  srt.block_id_offset = 1;
  for (int p = 0; p < 3; ++p) {
    if (p == 1 && !cm->lf.filter_level_u) continue;
    if (p == 2 && !cm->lf.filter_level_v) continue;
    srt.plane = p;
    srt.offset_base = blk_offsets[p];
    srt.block_cols = blk_cols[p];
    srt.wicount = (blk_count[p] >> 1) * 4;
    command_list->SetComputeRoot32BitConstants(3, 5, &srt, 0);
    command_list->Dispatch((srt.wicount + 63) >> 6, 1, 1);
  }

  command_list->ResourceBarrier(1, &CD3DX12_RESOURCE_BARRIER::UAV(dec->frame_buffer_pool->dev));
  av1_perf_gpu_mark(dec, AV1_PERF_GPU_LF_VERT);
  av1_stage_dump_fb(dec, "lf_vert", td->frame_buffer);

  shader = &td->shaders->loopfilter_h;
  command_list->SetPipelineState(shader->pso.Get());
  for (int p = 0; p < 3; ++p) {
    if (p == 1 && !cm->lf.filter_level_u) continue;
    if (p == 2 && !cm->lf.filter_level_v) continue;
    int type = p + 3;
    srt.plane = p;
    srt.offset_base = blk_offsets[type];
    srt.block_cols = blk_cols[type];
    srt.wicount = blk_count[type] * 4;
    command_list->SetComputeRoot32BitConstants(3, 5, &srt, 0);
    command_list->Dispatch((srt.wicount + 63) >> 6, 1, 1);
  }
  command_list->ResourceBarrier(1, &CD3DX12_RESOURCE_BARRIER::UAV(dec->frame_buffer_pool->dev));
  av1_perf_gpu_mark(dec, AV1_PERF_GPU_LF_HORZ);
  av1_stage_dump_fb(dec, "lf_horz", td->frame_buffer);
}

struct UpscaleData {
  int src_planes[3][4];
  int dst_planes[3][4];
};

struct UpscaleSRT {
  int plane;
  int wi_count;
  int hbd;
};

void av1_superres(Av1Core *dec, HwFrameBuffer *src, HwFrameBuffer *dst, int iter) {
  av1_frame_thread_data *td = dec->curr_frame_data;
  ComputeCommandBuffer *cb = &td->command_buffer;
  Microsoft::WRL::ComPtr<ID3D12GraphicsCommandList> command_list = dec->compute.command_list;
  ConstantBufferObject cbo = cb->Alloc(sizeof(UpscaleData));
  UpscaleData *data = (UpscaleData *)cbo.host_ptr;
  memcpy(data->src_planes, src->planes, sizeof(src->planes));
  memcpy(data->dst_planes, dst->planes, sizeof(dst->planes));
  data->dst_planes[0][2] = dst->y_crop_width;
  data->dst_planes[1][2] = dst->uv_crop_width;
  data->dst_planes[2][2] = dst->uv_crop_width;
  data->src_planes[0][2] = src->y_crop_width;
  data->src_planes[1][2] = src->uv_crop_width;
  data->src_planes[2][2] = src->uv_crop_width;
  data->src_planes[0][3] = src->width;
  data->src_planes[1][3] = (src->width + 1) >> 1;
  data->src_planes[2][3] = (src->width + 1) >> 1;

  UpscaleSRT srt;
  ComputeShader *shader = &dec->shader_lib->shader_upscale;
  command_list->SetPipelineState(shader->pso.Get());
  command_list->SetComputeRootSignature(shader->signaturePtr.Get());
  command_list->SetComputeRootUnorderedAccessView(0, dec->frame_buffer_pool->dev->GetGPUVirtualAddress());
  command_list->SetComputeRootConstantBufferView(1, cbo.dev_address);
  for (int plane = 0; plane < 3; ++plane) {
    srt.plane = plane;
    const int w = plane ? dst->uv_crop_width : dst->y_crop_width;
    const int h = plane ? dst->uv_crop_height : dst->y_crop_height;
    srt.wi_count = h * ((w + 3) >> 2);
    srt.hbd = dst->hbd;
    command_list->SetComputeRoot32BitConstants(2, 3, &srt, 0);
    command_list->Dispatch((srt.wi_count + 63) >> 6, 1, 1);
  }

  command_list->ResourceBarrier(1, &CD3DX12_RESOURCE_BARRIER::UAV(dec->frame_buffer_pool->dev));
  av1_perf_gpu_mark(dec, AV1_PERF_GPU_SUPERRES);
  av1_stage_dump_fb(dec, iter == 2 ? "superres_lr" : "superres", dst);
}

struct PlaneInfo1 {
  int stride;
  int offset;
  int width;
  int height;
};

struct UnitsInfo {
  int Rows;
  int Cols;
  int Size;
  int Stride;
};

struct PlaneRestorationData {
  PlaneInfo1 plane;
  UnitsInfo units;

  int pp_offset;
  int dst_offset;
  int Lr_buffer_offset;
  int subsampling;
  int hbd;
  int bit_depth;
  int pad[2];
};

struct RestorationData {
  int Sgr_Params[16][4];
};

struct CDefData {
  int plane[4];
  int uv_stride;
  int dst_offset[3];

  int uv_offset[2];
  int index_stride;
  int skips_stride;

  int pri_damping;
  int sec_damping;
  int pli;
  int hbd;

  int bit_depth;
  int _dummie[3];

  int cdef_directions[16][2][4];
  int cdef_strength[16][4];
  int cdef_uv_strength[16][4];
};

// Which CDEF shader runs. cdef_filter_v2.hlsl writes the same bytes as cdef_filter.hlsl with a different work
// split (one group per 64x64 luma block) and is 7 to 9 times faster on 4K 10-bit streams; it is the default.
// av1_set_cdef_v2(0) selects the original shader (test player flag -cdefv1).
static int g_cdef_v2 = 1;

extern "C" void av1_set_cdef_v2(int enable) { g_cdef_v2 = enable; }

void av1_cdef_filter_run(Av1Core *dec, AV1_COMMON *cm, HwFrameBuffer *src, HwFrameBuffer *dst) {
  av1_frame_thread_data *td = dec->curr_frame_data;
  if (!td->do_cdef) {
    PutPerfMarker(td, &td->perf_markers[4]);
    return;
  }

  const CdefInfo *const cdef_info = &cm->cdef_info;

  int *cdef_indexes = (int *)td->cdef_indexes->host_ptr;
  int *cdef_skips = (int *)td->cdef_skips->host_ptr;
  const int nvfb = (cm->mi_rows + MI_SIZE_64X64 - 1) / MI_SIZE_64X64;
  const int nhfb = (cm->mi_cols + MI_SIZE_64X64 - 1) / MI_SIZE_64X64;

  for (int fbr = 0; fbr < nvfb; fbr++) {
    for (int fbc = 0; fbc < nhfb; fbc++) {
      cdef_indexes[fbr * nhfb + fbc] =
          cm->mi_grid_visible[MI_SIZE_64X64 * fbr * cm->mi_stride + MI_SIZE_64X64 * fbc]->cdef_strength;
    }
  }
  for (int r = 0; r < AOMMIN(nvfb * 16, cm->mi_rows / 2); r++) {
    for (int c = 0; c < AOMMIN(nhfb * 16, cm->mi_cols / 2); c++) {
      cdef_skips[r * nhfb * 16 + c] = cm->mi_grid_visible[(r * 2 + 0) * cm->mi_stride + c * 2 + 0]->skip &&
                                      cm->mi_grid_visible[(r * 2 + 0) * cm->mi_stride + c * 2 + 1]->skip &&
                                      cm->mi_grid_visible[(r * 2 + 1) * cm->mi_stride + c * 2 + 0]->skip &&
                                      cm->mi_grid_visible[(r * 2 + 1) * cm->mi_stride + c * 2 + 1]->skip;
    }
  }

  ComputeCommandBuffer *cb = &td->command_buffer;
  ConstantBufferObject cbo = cb->Alloc(sizeof(CDefData));
  CDefData *data = reinterpret_cast<CDefData *>(cbo.host_ptr);

  /* Generated from gen_filter_tables.c. */
  const int cdef_directions_new[8][4] = {{-1, +1, -2, +2}, {0, +1, -1, +2}, {0, +1, 0, +2}, {0, +1, 1, +2},
                                         {1, +1, 2, +2},   {1, +0, 2, +1},  {1, +0, 2, +0}, {1, +0, 2, -1}};
  for (int i = 0; i < 8; i++) {
    data->cdef_directions[i][0][1] = cdef_directions_new[i][0];
    data->cdef_directions[i][0][0] = cdef_directions_new[i][1];
    data->cdef_directions[i][1][1] = cdef_directions_new[i][2];
    data->cdef_directions[i][1][0] = cdef_directions_new[i][3];
    data->cdef_directions[8 + i][0][1] = cdef_directions_new[i][0];
    data->cdef_directions[8 + i][0][0] = cdef_directions_new[i][1];
    data->cdef_directions[8 + i][1][1] = cdef_directions_new[i][2];
    data->cdef_directions[8 + i][1][0] = cdef_directions_new[i][3];
  }
  for (int i = 0; i < 16; i++) {
    data->cdef_strength[i][0] = cdef_info->cdef_strengths[i];
    data->cdef_uv_strength[i][0] = cdef_info->cdef_uv_strengths[i];
  }

  data->index_stride = nhfb;
  data->skips_stride = nhfb * 16;
  data->hbd = td->is_hbd;
  data->bit_depth = td->bitdepth;

  data->pri_damping = cdef_info->cdef_pri_damping;
  data->sec_damping = cdef_info->cdef_sec_damping;
  int w = src->width;
  int h = src->height;
  data->plane[0] = src->planes[0].stride;
  data->plane[1] = src->planes[0].offset;
  data->plane[2] = w;
  data->plane[3] = h;
  data->uv_stride = src->planes[1].stride;
  data->uv_offset[0] = src->planes[1].offset;
  data->uv_offset[1] = src->planes[2].offset;

  // srt.data->pli = i;
  data->dst_offset[0] = dst->planes[0].offset;
  data->dst_offset[1] = dst->planes[1].offset;
  data->dst_offset[2] = dst->planes[2].offset;

  Microsoft::WRL::ComPtr<ID3D12GraphicsCommandList> command_list = dec->compute.command_list;
  const int cdef_v2 = g_cdef_v2;
  ComputeShader *shader = cdef_v2 ? &dec->shader_lib->shader_cdef_filter_v2 : &dec->shader_lib->shader_cdef_filter;
  command_list->SetPipelineState(shader->pso.Get());
  command_list->SetComputeRootSignature(shader->signaturePtr.Get());
  command_list->SetComputeRootShaderResourceView(0, td->cdef_indexes->dev->GetGPUVirtualAddress());
  command_list->SetComputeRootShaderResourceView(1, td->cdef_skips->dev->GetGPUVirtualAddress());
  command_list->SetComputeRootUnorderedAccessView(2, dec->frame_buffer_pool->dev->GetGPUVirtualAddress());
  command_list->SetComputeRootConstantBufferView(3, cbo.dev_address);
  if (cdef_v2)
    command_list->Dispatch((w + 63) >> 6, (h + 63) >> 6, 1);  // same bindings, 64x64 luma per group
  else
    command_list->Dispatch((w + 31) >> 5, (h + 31) >> 5, 1);
  command_list->ResourceBarrier(1, &CD3DX12_RESOURCE_BARRIER::UAV(dec->frame_buffer_pool->dev));
  PutPerfMarker(td, &td->perf_markers[4]);
  av1_perf_gpu_mark(dec, AV1_PERF_GPU_CDEF);
  av1_stage_dump_fb(dec, "cdef", dst, src);
}

void av1_looprestoration(Av1Core *dec, AV1_COMMON *cm, void *lr_ctxt) {
  av1_frame_thread_data *td = dec->curr_frame_data;
  if (!td->do_loop_rest) {
    PutPerfMarker(td, &td->perf_markers[5]);
    return;
  }

  if (cm->rst_info[0].frame_restoration_type != RESTORE_NONE ||
      cm->rst_info[1].frame_restoration_type != RESTORE_NONE || cm->rst_info[2].frame_restoration_type != RESTORE_NONE)
    av1_loop_restoration_filter_frame_init((AV1LrStruct *)lr_ctxt, (YV12_BUFFER_CONFIG *)&cm->cur_frame->buf, cm, 0, 3);

  AV1LrStruct *loop_rest_ctxt = (AV1LrStruct *)lr_ctxt;
  int lr_ptr = 0;
  // PlaneRestorationData pl_data[3] = { 0, 0, 0 };
  int pl_width[3] = {0, 0, 0};
  int pl_height[3] = {0, 0, 0};
  HwFrameBuffer *dst_buffer = td->dst_frame_buffer;
  HwFrameBuffer *src_buffer = (td->do_cdef == td->do_superres) ? td->frame_buffer : &dec->back_buffer1;
  HwFrameBuffer *pp_buffer = td->do_superres ? &dec->back_buffer1 : td->frame_buffer;

  ComputeCommandBuffer *cb = &td->command_buffer;
  ConstantBufferObject cbo1 = cb->Alloc(sizeof(RestorationData));
  RestorationData *data = reinterpret_cast<RestorationData *>(cbo1.host_ptr);
  ConstantBufferObject cbo2 = cb->Alloc(sizeof(PlaneRestorationData) * 3);
  PlaneRestorationData *pl_data = reinterpret_cast<PlaneRestorationData *>(cbo2.host_ptr);

  for (int p = 0; p < 3; ++p) {
    int subsampling = p != 0;
    pl_width[p] = dst_buffer->width >> subsampling;
    pl_height[p] = dst_buffer->height >> subsampling;

    pl_data[p].plane.width = p ? dst_buffer->uv_crop_width : dst_buffer->y_crop_width;     // pl_width[p];
    pl_data[p].plane.height = p ? dst_buffer->uv_crop_height : dst_buffer->y_crop_height;  // pl_height[p];
    pl_data[p].plane.stride = dst_buffer->planes[p].stride;
    pl_data[p].plane.offset = src_buffer->planes[p].offset;
    pl_data[p].pp_offset = pp_buffer->planes[p].offset;
    pl_data[p].dst_offset = dst_buffer->planes[p].offset;
    pl_data[p].subsampling = p != 0;
    pl_data[p].hbd = td->is_hbd;
    pl_data[p].bit_depth = td->bitdepth;

    if (cm->rst_info[p].frame_restoration_type == RESTORE_NONE) {
      continue;
    }

    FilterFrameCtxt *ctx = &loop_rest_ctxt->ctxt[p];
    pl_data[p].units.Rows = ctx->rsi->vert_units_per_tile;
    pl_data[p].units.Cols = ctx->rsi->horz_units_per_tile;
    pl_data[p].units.Size = ctx->rsi->restoration_unit_size;
    pl_data[p].units.Stride = ctx->rsi->horz_units_per_tile;
    pl_data[p].Lr_buffer_offset = lr_ptr;

    int *lr_type = (int *)td->loop_rest_types->host_ptr;
    int *lr_wiener = (int *)td->loop_rest_wiener->host_ptr;
    for (int u = 0; u < ctx->rsi->units_per_tile; ++u) {
      RestorationUnitInfo *unit = ctx->rsi->unit_info + u;
      int *dst_type = lr_type + lr_ptr * 4;
      int *dst_wiener = lr_wiener + lr_ptr * 16;
      ++lr_ptr;
      dst_type[0] = unit->restoration_type;
      dst_type[1] = unit->sgrproj_info.xqd[0];
      dst_type[2] = unit->sgrproj_info.xqd[1];
      dst_type[3] = unit->sgrproj_info.ep;
      if (unit->restoration_type == RESTORE_WIENER) {
        for (int i = 0; i < 8; ++i) dst_wiener[i] = unit->wiener_info.hfilter[i];
        for (int i = 0; i < 8; ++i) dst_wiener[i + 8] = unit->wiener_info.vfilter[i];
      }
    }
  }

  {
    struct CBuffer {
      int do_restoration;
      int plane;
    } cBuffer;
    memcpy(data->Sgr_Params, sgr_params, sizeof(sgr_params));

    Microsoft::WRL::ComPtr<ID3D12GraphicsCommandList> command_list = dec->compute.command_list;
    ComputeShader *shader = &dec->shader_lib->shader_loop_rest;
    command_list->SetPipelineState(shader->pso.Get());
    command_list->SetComputeRootSignature(shader->signaturePtr.Get());
    command_list->SetComputeRootShaderResourceView(0, dec->frame_buffer_pool->dev->GetGPUVirtualAddress());
    command_list->SetComputeRootShaderResourceView(1, td->loop_rest_types->dev->GetGPUVirtualAddress());
    command_list->SetComputeRootShaderResourceView(2, td->loop_rest_wiener->dev->GetGPUVirtualAddress());
    command_list->SetComputeRootUnorderedAccessView(3, dec->frame_buffer_pool->dev->GetGPUVirtualAddress());
    command_list->SetComputeRootConstantBufferView(4, cbo1.dev_address);
    command_list->SetComputeRootConstantBufferView(5, cbo2.dev_address);

    for (int p = 0; p < 3; ++p) {
      int w = pl_width[p];
      int h = pl_height[p];
      cBuffer.do_restoration = cm->rst_info[p].frame_restoration_type != RESTORE_NONE;
      cBuffer.plane = p;
      command_list->SetComputeRoot32BitConstants(6, 2, &cBuffer, 0);
      command_list->Dispatch((w + 15) >> 4, (h + 3) >> 2, 1);
    }

    command_list->ResourceBarrier(1, &CD3DX12_RESOURCE_BARRIER::UAV(dec->frame_buffer_pool->dev));
  }
  PutPerfMarker(td, &td->perf_markers[5]);
  av1_perf_gpu_mark(dec, AV1_PERF_GPU_LOOP_REST);
  av1_stage_dump_fb(dec, "loop_rest", dst_buffer);
}

void av1_cdef_looprestoration(Av1Core *dec, AV1_COMMON *cm, void *lr_ctxt) {
  av1_frame_thread_data *td = dec->curr_frame_data;
  ComputeCommandBuffer *cb = &td->command_buffer;
  Microsoft::WRL::ComPtr<ID3D12GraphicsCommandList> command_list = dec->compute.command_list;

  PlaneRestorationData *pl_data[3] = {0, 0, 0};
  RestorationData *lr_data = NULL;
  HwFrameBuffer *cdef_dst = (td->do_loop_rest == td->do_superres) ? td->dst_frame_buffer : &dec->back_buffer1;
  HwFrameBuffer *dst_fb = td->dst_frame_buffer;

  av1_cdef_filter_run(dec, cm, td->frame_buffer, cdef_dst);

  if (td->do_superres) {
    const int h_border = 16;
    const int bpp = 1 + dst_fb->hbd;
    const int upscaled_width = cm->superres_upscaled_width;
    const int y_height = (dst_fb->height + 127) & ~127;
    const int superres_w = (upscaled_width + 127) & ~127;
    const int superres_y_stride = (superres_w + 2 * h_border) * bpp;
    const int superres_uv_stride = ((superres_w >> 1) + 2 * h_border) * bpp;
    const int superres_y_size = y_height * superres_y_stride;
    const int superres_uv_size = (y_height >> 1) * superres_uv_stride;
    if (td->do_loop_rest) {
      // upscale pre cdef frame (td->frame_buffer) to temp buffer
      HwFrameBuffer *dst = &dec->back_buffer1;
      assert(dst->size >= (superres_y_size + 2 * superres_uv_size));
      dst->y_crop_width = upscaled_width;
      dst->uv_crop_width = (upscaled_width + 1) >> 1;
      dst->y_crop_height = dst_fb->y_crop_height;
      dst->uv_crop_height = dst_fb->uv_crop_height;
      dst->planes[0].stride = superres_y_stride;
      dst->planes[1].stride = superres_uv_stride;
      dst->planes[2].stride = superres_uv_stride;
      dst->planes[0].offset = static_cast<int>(dst->base_offset + h_border * bpp);
      dst->planes[1].offset = dst->planes[0].offset + superres_y_size;
      dst->planes[2].offset = dst->planes[1].offset + superres_uv_size;
      dst->hbd = dst_fb->hbd;
      av1_superres(dec, td->frame_buffer, dst, 2);
    }

    if (td->do_cdef || (td->do_cdef == 0 && td->do_loop_rest == 0)) {
      // upscale post cdef frame (cdef_dst) or src (td->frame_buffer) to:
      HwFrameBuffer *src = td->do_cdef ? cdef_dst : td->frame_buffer;
      HwFrameBuffer *dst = td->do_loop_rest ? td->frame_buffer : dst_fb;
      assert(dst->size >= (superres_y_size + 2 * superres_uv_size));
      dst->y_crop_width = upscaled_width;
      dst->uv_crop_width = (upscaled_width + 1) >> 1;
      dst->y_crop_height = dst_fb->y_crop_height;
      dst->uv_crop_height = dst_fb->uv_crop_height;
      dst->planes[0].stride = superres_y_stride;
      dst->planes[1].stride = superres_uv_stride;
      dst->planes[2].stride = superres_uv_stride;
      dst->planes[0].offset = static_cast<int>(dst->base_offset + h_border * bpp);
      dst->planes[1].offset = dst->planes[0].offset + superres_y_size;
      dst->planes[2].offset = dst->planes[1].offset + superres_uv_size;
      dst->hbd = dst_fb->hbd;
      av1_superres(dec, src, dst, 3);
    }

    // update dst fb if not yet updated
    dst_fb->y_crop_width = upscaled_width;
    dst_fb->uv_crop_width = (upscaled_width + 1) >> 1;
    dst_fb->width = (upscaled_width + 7) & ~7;
    dst_fb->planes[0].stride = superres_y_stride;
    dst_fb->planes[1].stride = superres_uv_stride;
    dst_fb->planes[2].stride = superres_uv_stride;
    dst_fb->planes[0].offset = static_cast<int>(dst_fb->base_offset + h_border * bpp);
    dst_fb->planes[1].offset = dst_fb->planes[0].offset + superres_y_size;
    dst_fb->planes[2].offset = dst_fb->planes[1].offset + superres_uv_size;
    YV12_BUFFER_CONFIG *const buf = &cm->cur_frame->buf;
    buf->y_crop_width = dst_fb->y_crop_width;
    buf->uv_crop_width = dst_fb->uv_crop_width;
    buf->y_width = dst_fb->width;
    buf->uv_width = buf->y_width >> 1;
    /*
    YV12_BUFFER_CONFIG *const buf = &cm->cur_frame->buf;
    buf->y_crop_width = upscaled_width;
    buf->uv_crop_width = (upscaled_width + 1) >> 1;
    buf->y_width = (upscaled_width + 7) & ~7;
    buf->uv_width = buf->y_width >> 1;*/
  }

  av1_looprestoration(dec, cm, lr_ctxt);
}

struct GrainParams {
  // This structure is compared element-by-element in the function
  // av1_check_grain_params_equiv: this function must be updated if any changes
  // are made to this structure.
  int apply_grain;
  int update_parameters;
  int num_y_points;   // value: 0..14
  int num_cb_points;  // value: 0..10

  int num_cr_points;   // value: 0..10
  int scaling_shift;   // values : 8..11
  int ar_coeff_lag;    // values:  0..3
  int ar_coeff_shift;  // values : 6..9

  // Shift value: AR coeffs range
  // 6: [-2, 2)
  // 7: [-1, 1)
  // 8: [-0.5, 0.5)
  // 9: [-0.25, 0.25)

  int cb_mult;       // 8 bits
  int cb_luma_mult;  // 8 bits
  int cb_offset;     // 9 bits
  int cr_mult;       // 8 bits

  int cr_luma_mult;  // 8 bits
  int cr_offset;     // 9 bits
  int overlap_flag;
  int clip_to_restricted_range;

  unsigned int bit_depth;  // video bit depth
  int chroma_scaling_from_luma;
  int grain_scale_shift;
  unsigned int random_seed;

  // Y 8 bit values 14*2 = 7*4
  // UV 8 bit values 10*2 = 5*4+2 (padding 2)
  int scaling_points[14][2][4];

  // Y 8 bit values 24 = 6*4
  // UV 8 bit values 25 = 6*4 + 1 (padding 3)
  int ar_coeffs[3][25][4];  // xyz = y, cb, cr
};

struct FilmGrainGenData {
  GrainParams params;

  int luma_block_size_y;
  int luma_block_size_x;
  int luma_grain_stride;
  int chroma_block_size_y;

  int chroma_block_size_x;
  int chroma_grain_stride;
  int left_pad;
  int top_pad;

  int right_pad;
  int bottom_pad;
  int dst_offset_u;
  int dst_offset_v;

  int pred_pos[25][3][4];
};

struct FilmGrainData {
  GrainParams params;

  int src_planes[3][4];
  int dst_planes[3][4];

  int enable_chroma;
  int random_offset_stride;
  int width;
  int height;

  int mc_identity;
  int luma_grain_stride;
  int chroma_grain_stride;
  int left_pad;

  int right_pad;
  int top_pad;
  int bottom_pad;
  int ar_padding;

  int grain_offset_u;
  int grain_offset_v;
  int is_10x3;
  int pad;

  int scaling_lut[256][4];
};

static void init_scaling_function(const int scaling_points[][2], int num_points, int scaling_lut[][4], int p) {
  if (num_points == 0) {
    for (int i = 0; i < 256; i++) scaling_lut[i][p] = 0;
    return;
  }
  for (int i = 0; i < scaling_points[0][0]; i++) scaling_lut[i][p] = scaling_points[0][1];
  for (int point = 0; point < num_points - 1; point++) {
    int delta_y = scaling_points[point + 1][1] - scaling_points[point][1];
    int delta_x = scaling_points[point + 1][0] - scaling_points[point][0];
    int64_t delta = delta_y * ((65536 + (delta_x >> 1)) / delta_x);
    for (int x = 0; x < delta_x; x++) {
      scaling_lut[scaling_points[point][0] + x][p] = scaling_points[point][1] + (int)((x * delta + 32768) >> 16);
    }
  }
  for (int i = scaling_points[num_points - 1][0]; i < 256; i++) scaling_lut[i][p] = scaling_points[num_points - 1][1];
}

struct RNG {
  uint16_t random_register;

  void init(int luma_line, uint16_t seed) {
    uint16_t msb = (seed >> 8) & 255;
    uint16_t lsb = seed & 255;
    random_register = (msb << 8) + lsb;
    int luma_num = luma_line >> 5;
    random_register ^= ((luma_num * 37 + 178) & 255) << 8;
    random_register ^= ((luma_num * 173 + 105) & 255);
  }

  int get_random_number(int bits) {
    uint16_t bit;
    bit = ((random_register >> 0) ^ (random_register >> 1) ^ (random_register >> 3) ^ (random_register >> 12)) & 1;
    random_register = (random_register >> 1) | (bit << 15);
    return (random_register >> (16 - bits)) & ((1 << bits) - 1);
  }
};

void av1_filmgrain_run(Av1Core *dec, AV1_COMMON *cm, int enable_chorma) {
  av1_frame_thread_data *td = dec->curr_frame_data;
  aom_film_grain_t *params = &cm->film_grain_params;

  ComputeCommandBuffer *cb = &td->command_buffer;
  ConstantBufferObject cbo1 = cb->Alloc(sizeof(FilmGrainGenData));
  FilmGrainGenData *gen_data = reinterpret_cast<FilmGrainGenData *>(cbo1.host_ptr);

  gen_data->params.apply_grain = params->apply_grain;
  gen_data->params.ar_coeff_lag = params->ar_coeff_lag;
  gen_data->params.ar_coeff_shift = params->ar_coeff_shift;
  gen_data->params.bit_depth = params->bit_depth;
  gen_data->params.cb_luma_mult = params->cb_luma_mult;
  gen_data->params.cb_mult = params->cb_mult;
  gen_data->params.cb_offset = params->cb_offset;
  gen_data->params.chroma_scaling_from_luma = params->chroma_scaling_from_luma;
  gen_data->params.clip_to_restricted_range = params->clip_to_restricted_range;
  gen_data->params.cr_luma_mult = params->cr_luma_mult;
  gen_data->params.cr_mult = params->cr_mult;
  gen_data->params.cr_offset = params->cr_offset;
  gen_data->params.grain_scale_shift = params->grain_scale_shift;
  gen_data->params.num_cb_points = params->num_cb_points;
  gen_data->params.num_cr_points = params->num_cr_points;
  gen_data->params.num_y_points = params->num_y_points;
  gen_data->params.overlap_flag = params->overlap_flag;
  gen_data->params.random_seed = params->random_seed;
  gen_data->params.update_parameters = params->update_parameters;
  gen_data->params.scaling_shift = params->scaling_shift;
  for (int i = 0; i < 24; ++i) {
    gen_data->params.ar_coeffs[0][i][0] = params->ar_coeffs_y[i];
    gen_data->params.ar_coeffs[1][i][0] = params->ar_coeffs_cb[i];
    gen_data->params.ar_coeffs[2][i][0] = params->ar_coeffs_cr[i];
  }
  gen_data->params.ar_coeffs[1][24][0] = params->ar_coeffs_cb[24];
  gen_data->params.ar_coeffs[2][24][0] = params->ar_coeffs_cr[24];

  for (int i = 0; i < 14; ++i) {
    gen_data->params.scaling_points[i][0][0] = params->scaling_points_y[i][0];
    gen_data->params.scaling_points[i][1][0] = params->scaling_points_y[i][1];
    if (i < 10) {
      gen_data->params.scaling_points[i][0][1] = params->scaling_points_cb[i][0];
      gen_data->params.scaling_points[i][1][1] = params->scaling_points_cb[i][1];
      gen_data->params.scaling_points[i][0][2] = params->scaling_points_cr[i][0];
      gen_data->params.scaling_points[i][1][2] = params->scaling_points_cr[i][1];
    }
  }

  const int left_pad = 3;
  const int right_pad = 3;  // padding to offset for AR coefficients
  const int top_pad = 3;
  const int bottom_pad = 0;
  const int ar_padding = 3;  // maximum lag used for stabilization of AR coefficients
  const int luma_subblock_size_y = 32;
  const int luma_subblock_size_x = 32;
  const int chroma_subblock_size_y = luma_subblock_size_y >> 1;
  const int chroma_subblock_size_x = luma_subblock_size_x >> 1;
  const int luma_block_size_y = top_pad + 2 * ar_padding + luma_subblock_size_y * 2 + bottom_pad;
  const int luma_block_size_x = left_pad + 2 * ar_padding + luma_subblock_size_x * 2 + 2 * ar_padding + right_pad;
  const int chroma_block_size_y = top_pad + (2 >> 1) * ar_padding + chroma_subblock_size_y * 2 + bottom_pad;
  const int chroma_block_size_x =
      left_pad + (2 >> 1) * ar_padding + chroma_subblock_size_x * 2 + (2 >> 1) * ar_padding + right_pad;

  gen_data->luma_block_size_y = luma_block_size_y;
  gen_data->luma_block_size_x = luma_block_size_x;
  gen_data->luma_grain_stride = luma_block_size_x;
  gen_data->chroma_block_size_y = chroma_block_size_y;
  gen_data->chroma_block_size_x = chroma_block_size_x;
  gen_data->chroma_grain_stride = chroma_block_size_x;
  gen_data->left_pad = left_pad;
  gen_data->top_pad = top_pad;
  gen_data->right_pad = right_pad;
  gen_data->bottom_pad = bottom_pad;
  gen_data->dst_offset_u = luma_block_size_x * luma_block_size_y;
  gen_data->dst_offset_v = luma_block_size_x * luma_block_size_y + chroma_block_size_x * chroma_block_size_y;
  int pos_ar_index = 0;
  for (int row = -params->ar_coeff_lag; row < 0; row++) {
    for (int col = -params->ar_coeff_lag; col < params->ar_coeff_lag + 1; col++) {
      gen_data->pred_pos[pos_ar_index][0][0] = row;
      gen_data->pred_pos[pos_ar_index][1][0] = col;
      gen_data->pred_pos[pos_ar_index][2][0] = 0;

      gen_data->pred_pos[pos_ar_index][0][1] = row;
      gen_data->pred_pos[pos_ar_index][1][1] = col;
      gen_data->pred_pos[pos_ar_index][2][1] = 0;
      ++pos_ar_index;
    }
  }

  for (int col = -params->ar_coeff_lag; col < 0; col++) {
    gen_data->pred_pos[pos_ar_index][0][0] = 0;
    gen_data->pred_pos[pos_ar_index][1][0] = col;
    gen_data->pred_pos[pos_ar_index][2][0] = 0;

    gen_data->pred_pos[pos_ar_index][0][1] = 0;
    gen_data->pred_pos[pos_ar_index][1][1] = col;
    gen_data->pred_pos[pos_ar_index][2][1] = 0;
    ++pos_ar_index;
  }
  if (params->num_y_points > 0) {
    gen_data->pred_pos[pos_ar_index][0][1] = 0;
    gen_data->pred_pos[pos_ar_index][1][1] = 0;
    gen_data->pred_pos[pos_ar_index][2][1] = 1;
  }

  ConstantBufferObject cbo2 = cb->Alloc(sizeof(FilmGrainData));
  FilmGrainData *data = reinterpret_cast<FilmGrainData *>(cbo2.host_ptr);

  data->params = gen_data->params;
  data->enable_chroma = enable_chorma;

  memcpy(data->dst_planes, dec->back_buffer1.planes, sizeof(dec->back_buffer1.planes));

  HwFrameBuffer *src = td->dst_frame_buffer;
  memcpy(data->src_planes, src->planes, sizeof(src->planes));

  data->luma_grain_stride = luma_block_size_x;
  data->chroma_grain_stride = chroma_block_size_x;
  data->grain_offset_u = luma_block_size_x * luma_block_size_y;
  data->grain_offset_v = luma_block_size_x * luma_block_size_y + chroma_block_size_x * chroma_block_size_y;
  data->mc_identity = cm->cur_frame->buf.matrix_coefficients == AOM_CICP_MC_IDENTITY ? 1 : 0;
  data->left_pad = left_pad;
  data->right_pad = right_pad;
  data->top_pad = top_pad;
  data->bottom_pad = bottom_pad;
  data->ar_padding = ar_padding;
  data->width = src->y_crop_width;
  data->height = src->y_crop_height;
  data->random_offset_stride = (src->width / 32) + 1;
  data->is_10x3 = dec->tryhdr10x3 && src->hbd;
  // data->scaling_lut[256][4];
  init_scaling_function(params->scaling_points_y, params->num_y_points, data->scaling_lut, 0);
  if (params->chroma_scaling_from_luma) {
    for (int i = 0; i < 256; ++i) {
      data->scaling_lut[i][1] = data->scaling_lut[i][0];
      data->scaling_lut[i][2] = data->scaling_lut[i][0];
    }
  } else {
    init_scaling_function(params->scaling_points_cb, params->num_cb_points, data->scaling_lut, 1);
    init_scaling_function(params->scaling_points_cr, params->num_cr_points, data->scaling_lut, 2);
  }

  RNG rng;
  // const int random_offset_width = src->width / luma_subblock_size_x;
  int *random_offset = (int *)td->filmgrain_rand_offset->host_ptr;
  for (int y = 0; y < src->height / 2; y += (luma_subblock_size_y >> 1)) {
    rng.init(y * 2, params->random_seed);
    for (int x = 0; x < src->width / 2; x += (luma_subblock_size_x >> 1)) {
      random_offset[(y / (luma_subblock_size_x >> 1)) * data->random_offset_stride + x / (luma_subblock_size_x >> 1)] =
          rng.get_random_number(8);
    }
  }

  Microsoft::WRL::ComPtr<ID3D12GraphicsCommandList> command_list = dec->compute.command_list;
  ComputeShader *shader = &dec->shader_lib->shader_filmgrain_luma_gen;
  command_list->SetPipelineState(shader->pso.Get());
  command_list->SetComputeRootSignature(shader->signaturePtr.Get());
  command_list->SetComputeRootShaderResourceView(0, dec->filmgrain_random_luma->dev->GetGPUVirtualAddress());
  command_list->SetComputeRootShaderResourceView(1, dec->filmgrain_random_chroma->dev->GetGPUVirtualAddress());
  command_list->SetComputeRootShaderResourceView(2, dec->filmgrain_gaus->dev->GetGPUVirtualAddress());
  command_list->SetComputeRootUnorderedAccessView(3, dec->filmgrain_noise->dev->GetGPUVirtualAddress());
  command_list->SetComputeRootConstantBufferView(4, cbo1.dev_address);
  command_list->Dispatch(1, 1, 1);
  command_list->ResourceBarrier(1, &CD3DX12_RESOURCE_BARRIER::UAV(dec->filmgrain_noise->dev));

  shader = &dec->shader_lib->shader_filmgrain_chroma_gen;
  command_list->SetPipelineState(shader->pso.Get());
  command_list->SetComputeRootSignature(shader->signaturePtr.Get());
  command_list->SetComputeRootShaderResourceView(0, dec->filmgrain_random_luma->dev->GetGPUVirtualAddress());
  command_list->SetComputeRootShaderResourceView(1, dec->filmgrain_random_chroma->dev->GetGPUVirtualAddress());
  command_list->SetComputeRootShaderResourceView(2, dec->filmgrain_gaus->dev->GetGPUVirtualAddress());
  command_list->SetComputeRootUnorderedAccessView(3, dec->filmgrain_noise->dev->GetGPUVirtualAddress());
  command_list->SetComputeRootConstantBufferView(4, cbo1.dev_address);
  command_list->Dispatch(1, 2, 1);
  command_list->ResourceBarrier(1, &CD3DX12_RESOURCE_BARRIER::UAV(dec->filmgrain_noise->dev));

  shader = &dec->shader_lib->shader_filmgrain_filter;
  command_list->SetPipelineState(shader->pso.Get());
  command_list->SetComputeRootSignature(shader->signaturePtr.Get());
  command_list->SetComputeRootShaderResourceView(0, dec->frame_buffer_pool->dev->GetGPUVirtualAddress());
  command_list->SetComputeRootShaderResourceView(1, dec->filmgrain_noise->dev->GetGPUVirtualAddress());
  command_list->SetComputeRootShaderResourceView(2, td->filmgrain_rand_offset->dev->GetGPUVirtualAddress());
  command_list->SetComputeRootUnorderedAccessView(3, dec->frame_buffer_pool->dev->GetGPUVirtualAddress());
  command_list->SetComputeRootConstantBufferView(4, cbo2.dev_address);
  command_list->Dispatch(src->width / 32 + 1, src->height / 32 + 1, 1);
  command_list->ResourceBarrier(1, &CD3DX12_RESOURCE_BARRIER::UAV(dec->frame_buffer_pool->dev));

  PutPerfMarker(td, &td->perf_markers[7]);
  av1_perf_gpu_mark(dec, AV1_PERF_GPU_FILM_GRAIN);
}

struct CopyPlaneData {
  unsigned int cb_wi_count;
  unsigned int cb_src_offset;
  unsigned int cb_src_stride;
  unsigned int cb_dst_width;
  unsigned int cb_dst_offset;
  unsigned int cb_dst_stride;
};

// The output of a frame: takes an output image from the application and records the copy of the frame
// buffer into it, with film grain. Only for a frame that is output now: a frame with show_frame, at the end
// of its own command list, or a frame shown with show_existing_frame, in the command list of that
// show_existing_frame (av1_decode_sef; cm->show_frame is 1 there and cm->film_grain_params are the frame's).
//
// A frame that is decoded but not shown takes no output image. It used to take one when it was showable and
// kept it until a show_existing_frame, or until the frame left the reference map; the output images of the
// application were then held by frames that are not output, and a stream with as many hidden frames as the
// application has images could not decode any further (the decode thread waits here for an image that only
// a later show_existing_frame would hand over).
int av1_postprocess_copy_output(Av1Core *dec, AV1_COMMON *cm) {
  av1_frame_thread_data *td = dec->curr_frame_data;
  cm->cur_frame->buf.hw_show_image = NULL;
  if (!cm->show_frame) return 0;

  HwFrameBuffer *src = td->dst_frame_buffer;
  const int is_monochrome = cm->cur_frame->buf.monochrome;
  const int is_10x3 = dec->tryhdr10x3 && src->hbd;
  const int align1 = 255;
  int y_stride = 0;
  int uv_stride = 0;
  const int bpp = src->hbd ? (is_10x3 ? 4 : 2) : 1;
  int dst_y_texture_width = is_10x3 ? (src->y_crop_width + 2) / 3 : src->y_crop_width;
  int dst_uv_texture_width = is_10x3 ? (src->uv_crop_width + 2) / 3 : src->uv_crop_width;
  if (is_monochrome) {
    y_stride = ((dst_y_texture_width * bpp + align1) & (~align1));
  } else {
    uv_stride = ((dst_uv_texture_width * bpp + align1) & (~align1));
    y_stride = uv_stride * 2;
  }

  const int num_planes = is_monochrome ? 1 : 3;
  const int y_size = (y_stride * src->y_crop_height + 511) & ~511;
  const int uv_size = (uv_stride * src->uv_crop_height + 511) & ~511;
  const int size = y_size + (num_planes - 1) * uv_size;
  // callback here:
  av1_decoded_frame_buffer_t fb = {0};

  int err = 1;
  frame_buffer_type fb_type = td->is_hbd ? (is_10x3 ? fbt10x3 : fbt10bit) : fbt8bit;
  int64_t perf_t = av1_perf_ticks();
  if (dec->cb_get_output_image)
    err = dec->cb_get_output_image(dec->image_alloc_priv, size, src->y_crop_width, src->y_crop_height, fb_type, &fb);
  av1_perf_add(dec, AV1_PERF_CPU_WAIT_OUTPUT, perf_t);
  if (err || (fb.dx12_buffer == 0 && fb.dx12_texture[0] == 0)) {
    aom_internal_error(&cm->error, AOM_CODEC_MEM_ERROR, "Failed to allocate output frame.");
  }

  perf_t = av1_perf_ticks();
  HwOutputImage *img = (HwOutputImage *)MTQueueGet(&dec->image_pool);
  av1_perf_add(dec, AV1_PERF_CPU_WAIT_IMAGE, perf_t);
  img->fb_ptr = (uint8_t *)fb.buffer_host_ptr;
  img->alloc_priv = fb.priv;
  img->is_valid = 1;
  img->y_crop_width = src->y_crop_width;
  img->y_crop_height = src->y_crop_height;
  img->uv_crop_width = src->uv_crop_width;
  img->uv_crop_height = src->uv_crop_height;
  img->frame_number = src->frame_number;
  img->hbd = td->is_hbd;
  img->monochrome = is_monochrome;
  img->planes[0].stride = y_stride;
  img->planes[1].stride = uv_stride;
  img->planes[2].stride = uv_stride;
  img->planes[0].offset = 0;
  img->planes[1].offset = y_size;
  img->planes[2].offset = y_size + uv_size;
  img->is_valid = 1;

  if (is_monochrome) {
    img->planes[1].offset = 0;
    img->planes[2].offset = 0;
  }
  img->size = fb.buffer_size;
  cm->cur_frame->buf.hw_show_image = img;

  const int film_grain_chroma = td->do_filmgrain && !is_monochrome &&
                                (cm->film_grain_params.num_cb_points || cm->film_grain_params.chroma_scaling_from_luma);

  HwFrameBuffer *dst = &dec->back_buffer1;
  dst->planes[0].stride = img->planes[0].stride;
  dst->planes[1].stride = img->planes[1].stride;
  dst->planes[2].stride = img->planes[2].stride;
  dst->planes[0].offset = static_cast<int>(img->planes[0].offset + dst->base_offset);
  dst->planes[1].offset = static_cast<int>(img->planes[1].offset + dst->base_offset);
  dst->planes[2].offset = static_cast<int>(img->planes[2].offset + dst->base_offset);
  if (td->do_filmgrain) {
    av1_filmgrain_run(dec, cm, film_grain_chroma);
  }

  Microsoft::WRL::ComPtr<ID3D12GraphicsCommandList> command_list = dec->compute.command_list;
  if (!td->do_filmgrain || !film_grain_chroma) {
    ComputeShader *shader =
        is_10x3 ? &dec->shader_lib->shader_copy_plane_10bit10x3 : &dec->shader_lib->shader_copy_plane;
    command_list->SetComputeRootSignature(shader->signaturePtr.Get());
    command_list->SetPipelineState(shader->pso.Get());
    command_list->SetComputeRootUnorderedAccessView(0, dec->frame_buffer_pool->dev->GetGPUVirtualAddress());

    CopyPlaneData data;

    for (int plane = td->do_filmgrain; plane < num_planes; ++plane) {
      const int w = is_10x3 ? (((plane == 0 ? img->y_crop_width : img->uv_crop_width) + 23) / 24)
                            : (((plane == 0 ? img->y_crop_width : img->uv_crop_width) * bpp + 15) >> 4);
      const int h = plane == 0 ? img->y_crop_height : img->uv_crop_height;
      data.cb_wi_count = w * h;
      data.cb_src_offset = src->planes[plane].offset;
      data.cb_src_stride = src->planes[plane].stride;
      data.cb_dst_width = w;
      data.cb_dst_offset = dst->planes[plane].offset;
      data.cb_dst_stride = dst->planes[plane].stride;
      command_list->SetComputeRoot32BitConstants(1, 6, &data, 0);
      command_list->Dispatch((data.cb_wi_count + 63) >> 6, 1, 1);
    }
    command_list->ResourceBarrier(1, &CD3DX12_RESOURCE_BARRIER::UAV(dec->frame_buffer_pool->dev));
    PutPerfMarker(td, &td->perf_markers[6]);
    PutPerfMarker(td, &td->perf_markers[7]);
    av1_perf_gpu_mark(dec, AV1_PERF_GPU_COPY_PLANE);
  }

  command_list->ResourceBarrier(
      1, &CD3DX12_RESOURCE_BARRIER::Transition(dec->frame_buffer_pool->dev, D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
                                               D3D12_RESOURCE_STATE_COPY_SOURCE));
  if (fb.dx12_buffer) {
    command_list->CopyBufferRegion(static_cast<ID3D12Resource *>(fb.dx12_buffer), 0, dec->frame_buffer_pool->dev,
                                   dst->base_offset, size);
  }

  if (fb.dx12_texture[0]) {
    for (int i = 0; i < num_planes; i++) {
      ID3D12Resource *dst_texture = static_cast<ID3D12Resource *>(fb.dx12_texture[i]);
      if (!dst_texture) continue;
      UINT64 RequiredSize = 0;
      D3D12_PLACED_SUBRESOURCE_FOOTPRINT buffer_layout = {};
      buffer_layout.Offset = dst->planes[i].offset;
      buffer_layout.Footprint.Depth = 1;
      buffer_layout.Footprint.Format =
          img->hbd ? (is_10x3 ? DXGI_FORMAT_R10G10B10A2_UNORM : DXGI_FORMAT_R16_UNORM) : DXGI_FORMAT_R8_UNORM;
      buffer_layout.Footprint.Width = i ? dst_uv_texture_width : dst_y_texture_width;
      buffer_layout.Footprint.Height = i ? img->uv_crop_height : img->y_crop_height;
      buffer_layout.Footprint.RowPitch = dst->planes[i].stride;

      CD3DX12_TEXTURE_COPY_LOCATION Dst(dst_texture, 0);
      CD3DX12_TEXTURE_COPY_LOCATION Src(dec->frame_buffer_pool->dev, buffer_layout);
      command_list->CopyTextureRegion(&Dst, 0, 0, 0, &Src, nullptr);
    }
  }
  command_list->ResourceBarrier(
      1, &CD3DX12_RESOURCE_BARRIER::Transition(dec->frame_buffer_pool->dev, D3D12_RESOURCE_STATE_COPY_SOURCE,
                                               D3D12_RESOURCE_STATE_UNORDERED_ACCESS));
  av1_perf_gpu_mark(dec, AV1_PERF_GPU_OUTPUT_COPY);
  av1_stage_dump_output(dec, "output", img, (size_t)size);
  return 0;
}
