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
#include <wrl.h>
#include <d3d12.h>
#include <queue>
#include "aom_util/aom_thread.h"
#include "dx/av1_thread.h"

#define MAX_CREATE_SHADER_THREADS 6
typedef struct {
  Microsoft::WRL::ComPtr<ID3D12PipelineState> pso;
  Microsoft::WRL::ComPtr<ID3D12RootSignature> signaturePtr;
  const BYTE* bytecode;
  size_t size;
} ComputeShader;

typedef struct {
  ID3D12Device* device;
  const BYTE* src;
  int size;
  ID3D12RootSignature* sig;
  ComputeShader* dst;
} CreateShaderTask;

typedef struct {
  ComputeShader inter_base;
  ComputeShader inter_2x2;
  ComputeShader inter_comp;
  ComputeShader inter_comp_diff_y;
  ComputeShader inter_comp_diff_uv;
  ComputeShader inter_comp_masked;
  ComputeShader inter_comp_2x2;
  ComputeShader inter_warp;
  ComputeShader inter_obmc_above;
  ComputeShader inter_obmc_left;
  ComputeShader inter_warp_comp;
  ComputeShader intra_filter;
  ComputeShader intra_main;
  ComputeShader reconstruct_block;
  ComputeShader inter_ext_borders;
  ComputeShader loopfilter_v;
  ComputeShader loopfilter_h;
  ComputeShader inter_scale;
  ComputeShader inter_scale_2x2;
  ComputeShader inter_scale_comp;
  ComputeShader inter_scale_comp_diff_y;
  ComputeShader inter_scale_comp_diff_uv;
  ComputeShader inter_scale_comp_masked;
  ComputeShader inter_scale_comp_2x2;
  ComputeShader inter_scale_obmc_above;
  ComputeShader inter_scale_obmc_left;
  ComputeShader inter_scale_warp_comp;
} bitdepth_dependent_shaders;

typedef struct {
  ComputeShader shader_idct[20];
  ComputeShader shader_idct_sort;
  ComputeShader shader_loop_rest;
  ComputeShader shader_cdef_filter;
  ComputeShader shader_filmgrain_luma_gen;
  ComputeShader shader_filmgrain_chroma_gen;
  ComputeShader shader_filmgrain_filter;
  ComputeShader shader_copy_plane;
  ComputeShader shader_copy_plane_10bit10x3;
  ComputeShader shader_fill_buffer;
  ComputeShader shader_gen_pred_blocks;
  ComputeShader shader_gen_lf_vert;
  ComputeShader shader_gen_lf_hor;
  ComputeShader shader_upscale;
  bitdepth_dependent_shaders shaders_8bit;
  bitdepth_dependent_shaders shaders_hbd;

  Microsoft::WRL::ComPtr<ID3D12RootSignature> sig_idct;
  Microsoft::WRL::ComPtr<ID3D12RootSignature> sig_copy_plane;
  Microsoft::WRL::ComPtr<ID3D12RootSignature> sig_common111;
  Microsoft::WRL::ComPtr<ID3D12RootSignature> sig_common0102;
  Microsoft::WRL::ComPtr<ID3D12RootSignature> sig_common0110;
  Microsoft::WRL::ComPtr<ID3D12RootSignature> sig_pred_blocks;
  Microsoft::WRL::ComPtr<ID3D12RootSignature> sig_intra_pred;
  Microsoft::WRL::ComPtr<ID3D12RootSignature> sig_inter_pred;

  Microsoft::WRL::ComPtr<ID3D12RootSignature> sig_lf;
  Microsoft::WRL::ComPtr<ID3D12RootSignature> sig_lf_gen;
  Microsoft::WRL::ComPtr<ID3D12RootSignature> sig_loop_rest;
  Microsoft::WRL::ComPtr<ID3D12RootSignature> sig_cdef_filter;
  Microsoft::WRL::ComPtr<ID3D12RootSignature> sig_film_grain;
  Microsoft::WRL::ComPtr<ID3D12RootSignature> sig_upscale;

  int compliled_ = 0;
  int threads = 0;
  DataPtrQueueMT task_queue;
  pthread_t create_shader_threads[MAX_CREATE_SHADER_THREADS] = {};
  int create_shader_errors = 0;
  const int task_cnt = 0;
  CreateShaderTask* create_shader_tasks = 0;
} compute_shader_lib;

int wait_shader_create_complete(compute_shader_lib* lib);

// struct Av1Core;
HRESULT av1_upload_luts(struct Av1Core* dec);

static const int obmc_mask[16 * 4] = {
    // mask_2
    45, 64, 64, 64,
    // mask_4
    39, 50, 59, 64,
    // mask_8
    36, 42, 48, 53, 57, 61, 64, 64,
    // mask_16
    34, 37, 40, 43, 46, 49, 52, 54, 56, 58, 60, 61, 64, 64, 64, 64,
    // mask_32
    33, 35, 36, 38, 40, 41, 43, 44, 45, 47, 48, 50, 51, 52, 53, 55, 56, 57, 58, 59, 60, 60, 61, 62, 64, 64, 64, 64, 64,
    64, 64, 64};

static const int InterBlockSizeIndexLUT[6][6] = {
    // h:        4    8    16    32    64    128
    {0, 1, 2, -1, -1, -1},     // w = 4 (4)
    {3, 4, 5, 6, -1, -1},      // w = 8
    {7, 8, 9, 10, 11, -1},     // w = 16
    {-1, 12, 13, 14, 15, -1},  // w = 32
    {-1, -1, 16, 17, 18, 19},  // w = 64
    {-1, -1, -1, -1, 20, 21}   // w = 128
};
// TODO:
// 0        BLOCK_4X4
// 1        BLOCK_4X8    BLOCK_8X4
// 2        BLOCK_8X8    BLOCK_4X16    BLOCK_16X4
// 3        BLOCK_8X16    BLOCK_16X8
// 4        BLOCK_16X16    BLOCK_8X32    BLOCK_32X8
// 5        BLOCK_16X32    BLOCK_32X16
// 6        BLOCK_32X32    BLOCK_16X64    BLOCK_64X16
// 7        BLOCK_32X64    BLOCK_64X32
// 8        BLOCK_64X64
// 9        BLOCK_64X128    BLOCK_128X64
// 10    BLOCK_128X128
//
// const int InterBlockSizeIndexLUT[][] =
//{//h:        4    8    16    32    64    128
//        {    0,    1,    2,    -1,    -1,    -1},    //w = 4 (4)
//        {    1,    2,    3,    4,    -1,    -1},    //w = 8
//        {    2,    3,    4,    5,    6,    -1},    //w = 16
//        {    -1,    4,    5,    6,    7,    -1},    //w = 32
//        {    -1, -1, 6,    7,    8,    9},        //w = 64
//        {    -1, -1, -1, -1, 9,    10}        //w = 128
//};

const int InterBlockWidthLUT[] = {0, 0, 0, 1, 1, 1, 1, 2, 2, 2, 2, 2, 3, 3, 3, 3, 4, 4, 4, 4, 5, 5};
const int InterBlockHeightLUT[] = {0, 1, 2, 0, 1, 2, 3, 0, 1, 2, 3, 4, 1, 2, 3, 4, 2, 3, 4, 5, 4, 5};

const int dr_intra_derivative_reduced[28] = {0,  1023, 547, 372, 273, 215, 178, 151, 132, 116, 102, 90, 80, 71,
                                             64, 57,   51,  45,  40,  35,  31,  27,  23,  19,  15,  11, 7,  3};
const int intra_dx_index_lut[8][7] = {
    {25, 26, 27, 0, 27, 26, 25},  {3, 2, 1, 0, 0, 0, 0},  {11, 12, 13, 14, 15, 16, 17}, {17, 16, 15, 14, 13, 12, 11},
    {24, 23, 22, 21, 20, 19, 18}, {10, 9, 8, 7, 6, 5, 4}, {0, 0, 0, 0, 0, 0, 0},        {18, 19, 20, 21, 22, 23, 24}};

const int intra_dy_index_lut[8][7] = {
    {0, 0, 0, 0, 1, 2, 3},  {25, 26, 27, 0, 27, 26, 25},  {0, 0, 0, 0, 0, 0, 0},        {11, 12, 13, 14, 15, 16, 17},
    {4, 5, 6, 7, 8, 9, 10}, {18, 19, 20, 21, 22, 23, 24}, {24, 23, 22, 21, 20, 19, 18}, {0, 0, 0, 0, 0, 0, 0}};
const int intra_mode_shader_params[16 * 7][4] = {
    // dx, dy, flags, bits
    // DIR
    {11, 0, 305, 5},
    {7, 0, 305, 5},
    {3, 0, 305, 5},
    {0, 0, 273, 5},
    {-3, -1023, 850, 5},
    {-7, -547, 850, 5},
    {-11, -372, 850, 5},
    {-372, -11, 850, 5},
    {-547, -7, 850, 5},
    {-1023, -3, 850, 5},
    {0, 0, 323, 5},
    {0, 3, 451, 5},
    {0, 7, 451, 5},
    {0, 11, 451, 5},
    {90, 0, 305, 5},
    {80, 0, 305, 5},
    {71, 0, 305, 5},
    {64, 0, 305, 5},
    {57, 0, 305, 5},
    {51, 0, 305, 5},
    {45, 0, 305, 5},
    {-45, -90, 850, 5},
    {-51, -80, 850, 5},
    {-57, -71, 850, 5},
    {-64, -64, 850, 5},
    {-71, -57, 850, 5},
    {-80, -51, 850, 5},
    {-90, -45, 850, 5},
    {-15, -273, 850, 5},
    {-19, -215, 850, 5},
    {-23, -178, 850, 5},
    {-27, -151, 850, 5},
    {-31, -132, 850, 5},
    {-35, -116, 850, 5},
    {-40, -102, 850, 5},
    {-102, -40, 850, 5},
    {-116, -35, 850, 5},
    {-132, -31, 850, 5},
    {-151, -27, 850, 5},
    {-178, -23, 850, 5},
    {-215, -19, 850, 5},
    {-273, -15, 850, 5},
    {0, 15, 451, 5},
    {0, 19, 451, 5},
    {0, 23, 451, 5},
    {0, 27, 451, 5},
    {0, 31, 451, 5},
    {0, 35, 451, 5},
    {0, 40, 451, 5},
    {40, 0, 305, 5},
    {35, 0, 305, 5},
    {31, 0, 305, 5},
    {27, 0, 305, 5},
    {23, 0, 305, 5},
    {19, 0, 305, 5},
    {15, 0, 305, 5},
    // SMOOTH
    {0, 0, 92, 9},
    {0, 0, 92, 9},
    {0, 0, 92, 9},
    {0, 0, 92, 9},
    {0, 0, 92, 9},
    {0, 0, 92, 9},
    {0, 0, 92, 9},
    {0, 0, 84, 8},
    {0, 0, 84, 8},
    {0, 0, 84, 8},
    {0, 0, 84, 8},
    {0, 0, 84, 8},
    {0, 0, 84, 8},
    {0, 0, 84, 8},
    {0, 0, 88, 8},
    {0, 0, 88, 8},
    {0, 0, 88, 8},
    {0, 0, 88, 8},
    {0, 0, 88, 8},
    {0, 0, 88, 8},
    {0, 0, 88, 8},
    // PAETH
    {0, 0, 336, 5},
    {0, 0, 336, 5},
    {0, 0, 336, 5},
    {0, 0, 336, 5},
    {0, 0, 336, 5},
    {0, 0, 336, 5},
    {0, 0, 336, 5},
    // INTRA_BC
    {0, 0, 0, 0},
    {0, 0, 0, 0},
    {0, 0, 0, 0},
    {0, 0, 0, 0},
    {0, 0, 0, 0},
    {0, 0, 0, 0},
    {0, 0, 0, 0},
    // Filter?
    {0, 0, 80, 0},
    {0, 0, 80, 0},
    {0, 0, 80, 0},
    {0, 0, 80, 0},
    {0, 0, 80, 0},
    {0, 0, 80, 0},
    {0, 0, 80, 0},
    // DC:
    {0, 0, 80, 5},
    {0, 0, 80, 5},
    {0, 0, 80, 5},
    {0, 0, 80, 5},
    {0, 0, 80, 5},
    {0, 0, 80, 5},
    {0, 0, 80, 5},
    // CFL:
    {0, 0, 80, 5},
    {0, 0, 80, 5},
    {0, 0, 80, 5},
    {0, 0, 80, 5},
    {0, 0, 80, 5},
    {0, 0, 80, 5},
    {0, 0, 80, 5},
};

extern const int dx_gaussian_sequence[2048];

/*
int mode_params_lut[13][7][4];
for (int m = 0; m <= PAETH_PRED; ++m)
{
for (int delta = 0; delta < 7; ++delta)
{
int dir = m >= V_PRED && m <= D67_PRED;
int dx = 0;
int dy = 0;

int need_above = 1;
int need_left = 1;
int need_aboveleft = (m == PAETH_PRED); // | is_filter_mode;
int need_right = 0;
int need_bot = 0;
int do_corner_filt = 0;
if (dir)
{
    int angle = mode_to_angle_map[m] + (delta - 3) * 3;
    dir = (angle > 0 && angle <= 90) ? 1 :
        (angle > 90 && angle < 180) ? 2 :
        (angle >= 180 && angle < 270) ? 3 : 0;
    dx = (angle > 0 && angle < 90) ? dr_intra_derivative[angle] :
        (angle > 90 && angle < 180) ? -dr_intra_derivative[180 - angle] : 0;
    dy = (angle > 90 && angle < 180) ? -dr_intra_derivative[angle - 90] :
        (angle > 180 && angle < 270) ? dr_intra_derivative[270 - angle] : 0;
    need_aboveleft = 1;
    need_above = angle < 180;
    need_left = angle > 90;
    need_right = angle < 90;
    need_bot = angle > 180;
    do_corner_filt = need_above && need_left;
}

int m1 = m == 0 ? 12 : (m - 1);
mode_params_lut[m1][delta][0] = dx;
mode_params_lut[m1][delta][1] = dy;
mode_params_lut[m1][delta][2] = dir |
    ((m == SMOOTH_V_PRED || m == SMOOTH_PRED) ? 4 : 0) |
    ((m == SMOOTH_H_PRED || m == SMOOTH_PRED) ? 8 : 0) |
    (need_above << 4) |
    (need_right << 5) |
    (need_left << 6) |
    (need_bot << 7) |
    (need_aboveleft << 8) |
    (do_corner_filt << 9);
mode_params_lut[m1][delta][3] =
    (m == SMOOTH_H_PRED || m == SMOOTH_V_PRED) ? 8 :
    (m == SMOOTH_PRED) ? 9 : 5;
}
}
*/
