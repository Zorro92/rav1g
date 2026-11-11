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

uint init_random_generator(int luma_line, uint seed) {
  // same for the picture

  uint msb = (seed >> 8) & 255;
  uint lsb = seed & 255;

  uint random_register = (msb << 8) + lsb;

  //  changes for each row
  int luma_num = luma_line >> 5;

  random_register ^= ((luma_num * 37 + 178) & 255) << 8;
  random_register ^= ((luma_num * 173 + 105) & 255);
  return random_register;
}

#define WGH 64
groupshared int crcb_grain_temp[38 * 44];
groupshared int luma_grain_temp[70 * 76];

[numthreads(WGH, 1, 1)] void main(uint3 DTid
                                  : SV_DispatchThreadID, uint3 GTid
                                  : SV_GroupThreadID) {
  const int chroma_block_size_y = data.chroma_block_size_y;
  const int chroma_block_size_x = data.chroma_block_size_x;
  const int chroma_grain_stride = data.chroma_grain_stride;
  const int luma_grain_stride = data.luma_grain_stride;
  const int dst_offset_u = data.dst_offset_u;
  const int dst_offset_v = data.dst_offset_v;
  int bit_depth = data.params.bit_depth;
  int grain_center = 128 << (bit_depth - 8);
  int grain_min = 0 - grain_center;
  int grain_max = (256 << (bit_depth - 8)) - 1 - grain_center;
  int gauss_sec_shift = 12 - bit_depth + data.params.grain_scale_shift;

  int num_pos_chroma = 2 * data.params.ar_coeff_lag * (data.params.ar_coeff_lag + 1);
  if (data.params.num_y_points > 0) ++num_pos_chroma;
  int rounding_offset = (1 << (data.params.ar_coeff_shift - 1));
  int skip = 0;
  if (DTid.y == 0) {
    if (!(data.params.num_cb_points || data.params.chroma_scaling_from_luma)) {
      skip = 1;
    }
  } else {
    if (!(data.params.num_cr_points || data.params.chroma_scaling_from_luma)) {
      skip = 1;
    }
  }
  if (!skip) {
    if (GTid.x == 0) {
      uint rand_reg = init_random_generator((7 + DTid.y * 4) << 5, data.params.random_seed);
      rand_reg = get_random_number(rand_reg);
      crcb_grain_temp[0] = rand_reg;
      // could be optimizaed by 2^ setps parallelization
      for (int i = 1; i < chroma_block_size_y; i++) {
        rand_reg = random_uv[rand_reg];
        crcb_grain_temp[i * chroma_grain_stride] = rand_reg;
      }
    }
  }
  // One thread makes the random number every row starts from, then every thread makes those of its rows,
  // then all of them are turned into grain, each by the thread it falls to: three passes with a group sync
  // between them, outside the branch, as all threads of the group have to reach a group sync. (Upstream
  // had plain group memory barriers, which make no thread wait.)
  GroupMemoryBarrierWithGroupSync();
  if (!skip) {
    for (int i = GTid.x; i < chroma_block_size_y; i += WGH) {
      uint rand_reg = crcb_grain_temp[i * chroma_grain_stride];
      for (int j = 1; j < chroma_block_size_x; j++) {
        rand_reg = get_random_number(rand_reg);
        crcb_grain_temp[i * chroma_grain_stride + j] = rand_reg;
      }
    }
  }
  GroupMemoryBarrierWithGroupSync();
  if (!skip) {
    for (int j = GTid.x; j < chroma_block_size_y * chroma_block_size_x; j += WGH) {
      crcb_grain_temp[j] =
          (gaussian_sequence[clamp_rand(gauss_bits, crcb_grain_temp[j])] + ((1 << gauss_sec_shift) >> 1)) >>
          gauss_sec_shift;
    }
  } else {
    for (int j = GTid.x; j < chroma_block_size_y * chroma_block_size_x; j += WGH) crcb_grain_temp[j] = 0;
  }
  int i;
  for (i = 0; i < 70; i++) {
    for (int j = GTid.x; j < 76; j += WGH) {
      luma_grain_temp[i * 76 + j] = dst[(data.top_pad + i) * luma_grain_stride + j + data.left_pad];
    }
  }
  GroupMemoryBarrierWithGroupSync();
  // The filter of the grain template runs over its rows as a wavefront: a sample takes the samples to its
  // left and those of the rows above it, up to three columns to its right. Every thread has a row, and
  // starts on it four steps after the thread of the row above: in step n the thread t is at column
  // n - 4 * t (left_pad + 1 is 4), so what it reads was written in an earlier step. The threads of a group
  // run in no particular order, so the steps are kept apart by a group sync, which all threads of the group
  // have to reach: every thread makes all steps of the group, also those in which its row has not begun or
  // is done. (Upstream had every thread loop over its own row, the first columns skipped to make the
  // delay, and no barrier: that held only where the 64 threads of a group ran in lockstep, as one wave.)
  for (int first = data.top_pad; first < chroma_block_size_y - data.bottom_pad; first += WGH) {
    i = first + GTid.x;
    // The rows of this round, and its steps: the last row begins 4 * (rows - 1) steps after the first.
    const int rows = min(WGH, chroma_block_size_y - data.bottom_pad - first);
    const int steps = (rows - 1) * (data.left_pad + 1) + chroma_block_size_x - data.right_pad - data.left_pad;
    [loop] for (int step = 0; step < steps; ++step) {
      const int j = data.left_pad - GTid.x * (data.left_pad + 1) + step;
      if (i < chroma_block_size_y - data.bottom_pad && j >= data.left_pad && j < chroma_block_size_x - data.right_pad) {
        int wsum_crcb = 0;
        if (data.params.ar_coeff_lag == 3) {
          for (uint pos = 0; pos < 24; pos++) {
            wsum_crcb = wsum_crcb + data.params.ar_coeffs[1 + DTid.y][pos].x *
                                        crcb_grain_temp[(i + pos / 7 - 3) * chroma_grain_stride + j + pos % 7 - 3];
          }
        } else {
          for (int pos = 0; pos < num_pos_chroma - 1; pos++) {
            wsum_crcb =
                wsum_crcb +
                data.params.ar_coeffs[1 + DTid.y][pos].x *
                    crcb_grain_temp[(i + data.pred_pos[pos][0].y) * chroma_grain_stride + j + data.pred_pos[pos][1].y];
          }
        }
        if (data.params.num_y_points > 0) {
          int av_luma = 0;
          int luma_coord_y = ((i - data.top_pad) << 1);
          int luma_coord_x = ((j - data.left_pad) << 1);

          for (int k = luma_coord_y; k < luma_coord_y + 1 + 1; k++)
            for (int l = luma_coord_x; l < luma_coord_x + 1 + 1; l++) av_luma += luma_grain_temp[k * 76 + l];

          av_luma = (av_luma + ((1 << (1 + 1)) >> 1)) >> (1 + 1);

          wsum_crcb = wsum_crcb + data.params.ar_coeffs[DTid.y + 1][num_pos_chroma - 1].x * av_luma;
        }
        if (!skip)
          crcb_grain_temp[i * chroma_grain_stride + j] =
              clamp(crcb_grain_temp[i * chroma_grain_stride + j] +
                        ((wsum_crcb + rounding_offset) >> data.params.ar_coeff_shift),
                    grain_min, grain_max);
      }
      GroupMemoryBarrierWithGroupSync();
    }
  }
  GroupMemoryBarrierWithGroupSync();
  if (DTid.y == 0) {
    for (int j = GTid.x; j < chroma_block_size_y * chroma_block_size_x; j += WGH) {
      dst[dst_offset_u + j] = crcb_grain_temp[j];
    }
  } else {
    for (int j = GTid.x; j < chroma_block_size_y * chroma_block_size_x; j += WGH) {
      dst[dst_offset_v + j] = crcb_grain_temp[j];
    }
  }
}
