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

#include "film_grain_const.h"

StructuredBuffer<int> random : register(t0);
StructuredBuffer<int> random_uv : register(t1);
StructuredBuffer<int> gaussian_sequence : register(t2);
RWStructuredBuffer<int> dst : register(u0);

cbuffer cb_film_grain_data : register(b0) { FilmGrainGenData data; };

uint get_random_number(uint random_register_new) {
  uint bit;
  bit = ((random_register_new >> 0) ^ (random_register_new >> 1) ^ (random_register_new >> 3) ^
         (random_register_new >> 12)) &
        1;
  return (random_register_new >> 1) | (bit << 15);
}
int clamp_rand(int bits, uint rand) { return (rand >> (16 - bits)) & ((1 << bits) - 1); }

groupshared int luma_grain_temp[73 * 82];
#define WGH 64
[numthreads(WGH, 1, 1)] void main(uint3 thread
                                  : SV_DispatchThreadID) {
  const int luma_block_size_x = data.luma_block_size_x;
  const int luma_block_size_y = data.luma_block_size_y;
  const int luma_grain_stride = data.luma_grain_stride;

  if (data.params.num_y_points == 0) {
    for (int j = thread.x; j < luma_block_size_x * luma_block_size_y; j += WGH) dst[j] = 0;
    return;
  }
  int bit_depth = data.params.bit_depth;
  int grain_center = 128 << (bit_depth - 8);
  int grain_min = 0 - grain_center;
  int grain_max = (256 << (bit_depth - 8)) - 1 - grain_center;
  int gauss_sec_shift = 12 - bit_depth + data.params.grain_scale_shift;

  int rounding_offset = (1 << (data.params.ar_coeff_shift - 1));
  int num_pos_luma = 2 * data.params.ar_coeff_lag * (data.params.ar_coeff_lag + 1);

  if (thread.x == 0) {
    uint rand_reg = get_random_number(data.params.random_seed);
    luma_grain_temp[0] = rand_reg;
    // could be optimizaed by 2^ setps parallelization
    for (int i = 1; i < luma_block_size_y; i++) {
      rand_reg = random[rand_reg];
      luma_grain_temp[i * luma_grain_stride] = rand_reg;
    }
  }
  GroupMemoryBarrierWithGroupSync();
  int i, j;
  for (i = thread.x; i < luma_block_size_y; i += WGH) {
    uint rand_reg = luma_grain_temp[i * luma_grain_stride];
    for (int j = 1; j < luma_block_size_x; j++) {
      rand_reg = get_random_number(rand_reg);
      luma_grain_temp[i * luma_grain_stride + j] = rand_reg;
    }
  }
  GroupMemoryBarrierWithGroupSync();
  for (j = thread.x; j < luma_block_size_x * luma_block_size_y; j += WGH) {
    luma_grain_temp[j] =
        (gaussian_sequence[clamp_rand(gauss_bits, luma_grain_temp[j])] + ((1 << gauss_sec_shift) >> 1)) >>
        gauss_sec_shift;
  }
  GroupMemoryBarrierWithGroupSync();
  for (i = data.top_pad + (thread.x); i < luma_block_size_y - data.bottom_pad; i += WGH) {
    for (int j = data.left_pad - (thread.x) * (data.left_pad + 1); j < luma_block_size_x - data.right_pad; j++) {
      if (j < data.left_pad) {
        continue;
      }
      int wsum = 0;
      int offset = (i - 3) * luma_grain_stride + j - 3;
      if (data.params.ar_coeff_lag == 3) {
        for (uint pos = 0; pos < 24; pos++) {
          wsum += data.params.ar_coeffs[0][pos].x * luma_grain_temp[offset + (pos / 7) * luma_grain_stride + pos % 7];
        }
      } else {
        for (int pos = 0; pos < num_pos_luma; pos++) {
          wsum += data.params.ar_coeffs[0][pos].x *
                  luma_grain_temp[(i + data.pred_pos[pos][0].x) * luma_grain_stride + j + data.pred_pos[pos][1].x];
        }
      }

      int val =
          clamp(luma_grain_temp[i * luma_grain_stride + j] + ((wsum + rounding_offset) >> data.params.ar_coeff_shift),
                grain_min, grain_max);

      luma_grain_temp[i * luma_grain_stride + j] = val;
    }
  }
  GroupMemoryBarrierWithGroupSync();
  for (j = thread.x; j < luma_block_size_x * luma_block_size_y; j += WGH) dst[j] = luma_grain_temp[j];
}