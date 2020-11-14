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

ByteAddressBuffer src : register(t0);
StructuredBuffer<int> grain_block : register(t1);
StructuredBuffer<int> random_offset : register(t2);
RWByteAddressBuffer dst : register(u0);

struct FilmGrainData {
  GrainParams params;

  int4 src_planes[3];
  int4 dst_planes[3];

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

  int4 scaling_lut[256];
};

cbuffer cb_film_grain_data : register(b0) { FilmGrainData data; };

#define clamp_ln0(a, b) clamp((a * 23 + b * 22 + 16) >> 5, grain_min, grain_max)
#define clamp_ln1(a, b) clamp((a * 27 + b * 17 + 16) >> 5, grain_min, grain_max)
#define clamp_ln2(a, b) clamp((a * 17 + b * 27 + 16) >> 5, grain_min, grain_max)

#define clamp_ln(n, a, b) ((n == 1) ? clamp_ln1(a, b) : clamp_ln2(a, b))

groupshared int luma_grain_temp[3][32 * 32];
groupshared int avarage_luma[3][32 * 32];
groupshared int cr_grain_temp[3][16 * 16];
groupshared int cb_grain_temp[3][16 * 16];

#define chroma_subsamp_x 1
#define chroma_subsamp_y 1

//    return scaling_lut[x] + (((scaling_lut[x + 1] - scaling_lut[x]) *
//                                  (index & ((1 << (bit_depth - 8)) - 1)) +
//                              (1 << (bit_depth - 9))) >>
//                             (bit_depth - 8));

#define scale_LUT(index)                                                                                          \
  (data.scaling_lut[index >> (bit_depth - 8)].x +                                                                 \
   ((index >> (bit_depth - 8)) == 255                                                                             \
        ? 0                                                                                                       \
        : (((data.scaling_lut[(index >> (bit_depth - 8)) + 1].x - data.scaling_lut[index >> (bit_depth - 8)].x) * \
                (index & ((1 << (bit_depth - 8)) - 1)) +                                                          \
            (1 << (bit_depth - 9))) >>                                                                            \
           (bit_depth - 8))))

#define scale_LUT_cb(index)                                                                                       \
  (data.scaling_lut[index >> (bit_depth - 8)].y +                                                                 \
   ((index >> (bit_depth - 8)) == 255                                                                             \
        ? 0                                                                                                       \
        : (((data.scaling_lut[(index >> (bit_depth - 8)) + 1].y - data.scaling_lut[index >> (bit_depth - 8)].y) * \
                (index & ((1 << (bit_depth - 8)) - 1)) +                                                          \
            (1 << (bit_depth - 9))) >>                                                                            \
           (bit_depth - 8))))

#define scale_LUT_cr(index)                                                                                       \
  (data.scaling_lut[index >> (bit_depth - 8)].z +                                                                 \
   ((index >> (bit_depth - 8)) == 255                                                                             \
        ? 0                                                                                                       \
        : (((data.scaling_lut[(index >> (bit_depth - 8)) + 1].z - data.scaling_lut[index >> (bit_depth - 8)].z) * \
                (index & ((1 << (bit_depth - 8)) - 1)) +                                                          \
            (1 << (bit_depth - 9))) >>                                                                            \
           (bit_depth - 8))))

[numthreads(luma_subblock_size_x / 4, luma_subblock_size_y, 3)] void main(int3 Gid
                                                                          : SV_GroupID, int3 GTid
                                                                          : SV_GroupThreadID) {
  int overlap = data.params.overlap_flag;
  int bit_depth = data.params.bit_depth;
  int lid = GTid.z;
  int ii = Gid.y;
  int jj = Gid.x * 3 + lid;
  int grain_center = 128 << (bit_depth - 8);
  int grain_min = 0 - grain_center;
  int grain_max = (256 << (bit_depth - 8)) - 1 - grain_center;
  const int enable_chroma = data.enable_chroma;

  {
    int y = ii * luma_subblock_size_y;
    {
      int x = jj * luma_subblock_size_x;

      int true_chroma_subblock_size_y = luma_subblock_size_y >> chroma_subsamp_y;
      int true_chroma_subblock_size_x = luma_subblock_size_x >> chroma_subsamp_x;
      // Grain blocks offset calculation
      const int random_offset_stride = data.random_offset_stride;
      int offset_y_up = y ? random_offset[(ii - (y ? 1 : 0)) * random_offset_stride + jj] : 0;
      int offset_y_up_left =
          (y > 0) && (x > 0) ? random_offset[(ii - (y ? 1 : 0)) * random_offset_stride + jj - (x ? 1 : 0)] : 0;
      int offset_y_left = x ? random_offset[ii * random_offset_stride + jj - (x ? 1 : 0)] : 0;
      int offset_y = random_offset[ii * random_offset_stride + jj];
      int offset_x_up = (offset_y_up >> 4) & 15;
      int offset_x_up_left = (offset_y_up_left >> 4) & 15;
      int offset_x_left = (offset_y_left >> 4) & 15;
      int offset_x = (offset_y >> 4) & 15;
      offset_y_up &= 15;
      offset_y_up_left &= 15;
      offset_y_left &= 15;
      offset_y &= 15;
      const int ar_padding = data.ar_padding;
      int luma_offset_y = 2 * ar_padding + (offset_y << 1);
      int luma_offset_x = 2 * ar_padding + (offset_x << 1);
      int luma_offset_y_left = 2 * ar_padding + (offset_y_left << 1);
      int luma_offset_x_left = 2 * ar_padding + (offset_x_left << 1) + 32;
      int luma_offset_y_up = 2 * ar_padding + (offset_y_up << 1) + 32;
      int luma_offset_x_up = 2 * ar_padding + (offset_x_up << 1);
      int luma_offset_y_up_left = 2 * ar_padding + (offset_y_up_left << 1) + 32;
      int luma_offset_x_up_left = 2 * ar_padding + (offset_x_up_left << 1) + 32;

      const int top_pad = data.top_pad;
      const int left_pad = data.left_pad;
      int chroma_offset_y = top_pad + (luma_offset_y >> chroma_subsamp_y);
      int chroma_offset_x = left_pad + (luma_offset_x >> chroma_subsamp_x);
      int chroma_offset_y_left = top_pad + (luma_offset_y_left >> chroma_subsamp_y);
      int chroma_offset_x_left = left_pad + (luma_offset_x_left >> chroma_subsamp_x);
      int chroma_offset_y_up = top_pad + (luma_offset_y_up >> chroma_subsamp_y);
      int chroma_offset_x_up = left_pad + (luma_offset_x_up >> chroma_subsamp_x);
      int chroma_offset_y_up_left = top_pad + (luma_offset_y_up_left >> chroma_subsamp_y);
      int chroma_offset_x_up_left = left_pad + (luma_offset_x_up_left >> chroma_subsamp_x);

      luma_offset_y += top_pad;
      luma_offset_x += left_pad;
      luma_offset_y_left += top_pad;
      luma_offset_x_left += left_pad;
      luma_offset_y_up += top_pad;
      luma_offset_x_up += left_pad;
      luma_offset_y_up_left += top_pad;
      luma_offset_x_up_left += left_pad;

      const int grain_offset_u = data.grain_offset_u;
      const int grain_offset_v = data.grain_offset_v;
      const int luma_grain_stride = data.luma_grain_stride;
      const int chroma_grain_stride = data.chroma_grain_stride;
      // Grain blocks fetching
      //            for (int i = 0; i < 32; i++) {
      {
        int i = GTid.y;
        for (int j = GTid.x; j < luma_subblock_size_x; j += luma_subblock_size_x / 4) {
          // Luma grain fetching
          luma_grain_temp[lid][i * luma_subblock_size_x + j] =
              (grain_block[(luma_offset_y + i) * luma_grain_stride + luma_offset_x + j]);
          // Chroma grain fetching
          if (i < true_chroma_subblock_size_y && j < true_chroma_subblock_size_x && enable_chroma) {
            cb_grain_temp[lid][i * true_chroma_subblock_size_x + j] =
                (grain_block[grain_offset_u + (chroma_offset_y + i) * chroma_grain_stride + chroma_offset_x + j]);
            cr_grain_temp[lid][i * true_chroma_subblock_size_x + j] =
                grain_block[grain_offset_v + (chroma_offset_y + i) * chroma_grain_stride + chroma_offset_x + j];
          }
        }
      }
      GroupMemoryBarrierWithGroupSync();

      // Overlap processing on X axis
      if (overlap && x) {
        int i = GTid.y;
        int j = GTid.x;
        // Luma overlap
        if (j < 2) {
          int test_luma_left = (grain_block[(luma_offset_y_left + i) * luma_grain_stride + luma_offset_x_left + j]);
          luma_grain_temp[lid][i * luma_subblock_size_x + j] =
              clamp_ln(j + 1, test_luma_left, luma_grain_temp[lid][i * luma_subblock_size_x + j]);
        }
        // Chroma overlap
        if (i < true_chroma_subblock_size_y && j <= (1 - chroma_subsamp_x) && enable_chroma) {
          int test_cb_left = (grain_block[grain_offset_u + (chroma_offset_y_left + i) * chroma_grain_stride +
                                          chroma_offset_x_left + j]);
          int test_cr_left = (grain_block[grain_offset_v + (chroma_offset_y_left + i) * chroma_grain_stride +
                                          chroma_offset_x_left + j]);
          cb_grain_temp[lid][i * chroma_subblock_size_x + j] =
              clamp_ln0(test_cb_left, cb_grain_temp[lid][i * true_chroma_subblock_size_x + j]);
          cr_grain_temp[lid][i * chroma_subblock_size_x + j] =
              clamp_ln0(test_cr_left, cr_grain_temp[lid][i * true_chroma_subblock_size_x + j]);
        }
      }
      GroupMemoryBarrierWithGroupSync();

      // Overlap processing on Y axis
      if (overlap && y) {
        int i = GTid.y;
        for (int j = GTid.x; j < 32; j += luma_subblock_size_x / 4) {
          // Luma overlap
          if (i < 2) {
            int test_luma_up_left =
                (grain_block[(luma_offset_y_up_left + i) * luma_grain_stride + luma_offset_x_up_left + j]);
            int test_luma_up = (grain_block[(luma_offset_y_up + i) * luma_grain_stride + luma_offset_x_up + j]);
            if (x && (j < 2)) {
              test_luma_up = clamp_ln(j + 1, test_luma_up_left, test_luma_up);
            }
            luma_grain_temp[lid][i * luma_subblock_size_x + j] =
                clamp_ln(i + 1, test_luma_up, luma_grain_temp[lid][i * luma_subblock_size_x + j]);
          }
          // Chroma overlap
          if ((i <= (1 - chroma_subsamp_y)) && (j < true_chroma_subblock_size_x) && enable_chroma) {
            int test_cb_up_left = (grain_block[grain_offset_u + (chroma_offset_y_up_left + i) * chroma_grain_stride +
                                               chroma_offset_x_up_left + j]);

            int test_cb_up =
                (grain_block[grain_offset_u + (chroma_offset_y_up + i) * chroma_grain_stride + chroma_offset_x_up + j]);

            int test_cr_up_left = (grain_block[grain_offset_v + (chroma_offset_y_up_left + i) * chroma_grain_stride +
                                               chroma_offset_x_up_left + j]);

            int test_cr_up =
                (grain_block[grain_offset_v + (chroma_offset_y_up + i) * chroma_grain_stride + chroma_offset_x_up + j]);

            if (x && (j == 0)) {
              test_cb_up = clamp_ln0(test_cb_up_left, test_cb_up);
              test_cr_up = clamp_ln0(test_cr_up_left, test_cr_up);
            }

            cb_grain_temp[lid][i * true_chroma_subblock_size_x + j] =
                clamp_ln0(test_cb_up, cb_grain_temp[lid][i * true_chroma_subblock_size_x + j]);
            cr_grain_temp[lid][i * true_chroma_subblock_size_x + j] =
                clamp_ln0(test_cr_up, cr_grain_temp[lid][i * true_chroma_subblock_size_x + j]);
          }
        }
      }
      GroupMemoryBarrierWithGroupSync();

      // Grain blocks application
      int rounding_offset = (1 << (data.params.scaling_shift - 1));
      int min_luma, max_luma, min_chroma, max_chroma;

      if (data.params.clip_to_restricted_range) {
        min_luma = min_luma_legal_range << (bit_depth - 8);
        max_luma = max_luma_legal_range << (bit_depth - 8);
        if (data.mc_identity) {
          min_chroma = min_luma_legal_range << (bit_depth - 8);
          max_chroma = max_luma_legal_range << (bit_depth - 8);
        } else {
          min_chroma = min_chroma_legal_range << (bit_depth - 8);
          max_chroma = max_chroma_legal_range << (bit_depth - 8);
        }
      } else {
        min_luma = min_chroma = 0;
        max_luma = max_chroma = (256 << (bit_depth - 8)) - 1;
      }
      int cb_mult = data.params.cb_mult - 128;            // fixed scale
      int cb_luma_mult = data.params.cb_luma_mult - 128;  // fixed scale
      int cb_offset = (data.params.cb_offset << (bit_depth - 8)) - (1 << bit_depth);

      int cr_mult = data.params.cr_mult - 128;            // fixed scale
      int cr_luma_mult = data.params.cr_luma_mult - 128;  // fixed scale
      int cr_offset = (data.params.cr_offset << (bit_depth - 8)) - (1 << bit_depth);

      if (data.params.chroma_scaling_from_luma) {
        cb_mult = 0;        // fixed scale
        cb_luma_mult = 64;  // fixed scale
        cb_offset = 0;

        cr_mult = 0;        // fixed scale
        cr_luma_mult = 64;  // fixed scale
        cr_offset = 0;
      }
      int apply_y = data.params.num_y_points > 0 ? 1 : 0;
      int apply_cb = (data.params.num_cb_points > 0 || data.params.chroma_scaling_from_luma) ? 1 : 0;
      int apply_cr = (data.params.num_cr_points > 0 || data.params.chroma_scaling_from_luma) ? 1 : 0;

      // for (int i = 0; i < (luma_subblock_size_y); i++) {
      {
        // for (int j = 0; j < (luma_subblock_size_x); j += 4) {
        {
          // Luma grain block application
          int i = GTid.y;
          int j = GTid.x * 4;

          const int2 src_luma_plane = data.src_planes[0].xy;
          int4 in_luma;
          if (bit_depth == 8) {
            uint luma_uint = src.Load(src_luma_plane.y + (y + i) * src_luma_plane.x + x + j);
            in_luma.x = (luma_uint >> 0) & 255;
            in_luma.y = (luma_uint >> 8) & 255;
            in_luma.z = (luma_uint >> 16) & 255;
            in_luma.w = (luma_uint >> 24) & 255;
          } else {
            uint2 luma_uint = src.Load2(src_luma_plane.y + (y + i) * src_luma_plane.x + (x + j) * 2);
            in_luma.x = (luma_uint.x >> 0) & 0x03ff;
            in_luma.y = (luma_uint.x >> 16) & 0x03ff;
            in_luma.z = (luma_uint.y >> 0) & 0x03ff;
            in_luma.w = (luma_uint.y >> 16) & 0x03ff;
          }
          int4 scaled_luma;

          if (bit_depth == 8) {
            scaled_luma.x = data.scaling_lut[in_luma.x].x * luma_grain_temp[lid][i * luma_subblock_size_x + j + 0];
            scaled_luma.y = data.scaling_lut[in_luma.y].x * luma_grain_temp[lid][i * luma_subblock_size_x + j + 1];
            scaled_luma.z = data.scaling_lut[in_luma.z].x * luma_grain_temp[lid][i * luma_subblock_size_x + j + 2];
            scaled_luma.w = data.scaling_lut[in_luma.w].x * luma_grain_temp[lid][i * luma_subblock_size_x + j + 3];
          } else {
            scaled_luma.x = scale_LUT(in_luma.x) * luma_grain_temp[lid][i * luma_subblock_size_x + j + 0];
            scaled_luma.y = scale_LUT(in_luma.y) * luma_grain_temp[lid][i * luma_subblock_size_x + j + 1];
            scaled_luma.z = scale_LUT(in_luma.z) * luma_grain_temp[lid][i * luma_subblock_size_x + j + 2];
            scaled_luma.w = scale_LUT(in_luma.w) * luma_grain_temp[lid][i * luma_subblock_size_x + j + 3];
          }
          int4 out_luma =
              clamp(in_luma + ((scaled_luma + rounding_offset) >> data.params.scaling_shift), min_luma, max_luma);
          if (!apply_y) {
            out_luma = in_luma;
          }

          if (data.is_10x3) {
            luma_grain_temp[lid][i * luma_subblock_size_x + j + 0] = out_luma.x;
            luma_grain_temp[lid][i * luma_subblock_size_x + j + 1] = out_luma.y;
            luma_grain_temp[lid][i * luma_subblock_size_x + j + 2] = out_luma.z;
            luma_grain_temp[lid][i * luma_subblock_size_x + j + 3] = out_luma.w;
          } else {
            const int2 dst_luma_plane = data.dst_planes[0].xy;
            if (((y + i) < data.height) && ((x + j) < data.width)) {
              if (bit_depth == 8) {
                dst.Store(dst_luma_plane.y + (y + i) * dst_luma_plane.x + x + j,
                          out_luma.x | (out_luma.y << 8) | (out_luma.z << 16) | (out_luma.w << 24));
              } else {
                dst.Store2(dst_luma_plane.y + (y + i) * dst_luma_plane.x + (x + j) * 2,
                           uint2((out_luma.x << 0) | (out_luma.y << 16), (out_luma.z << 0) | (out_luma.w << 16)));
              }
            }
          }

          GroupMemoryBarrierWithGroupSync();

          if (data.is_10x3) {
            int x3 = Gid.x * luma_subblock_size_x * 3;
            const int2 dst_luma_plane = data.dst_planes[0].xy;
            for (int j3 = GTid.z * (luma_subblock_size_x / 4) + GTid.x; j3 < luma_subblock_size_x;
                 j3 += 3 * (luma_subblock_size_x / 4)) {
              uint3 res;
              res.x = luma_grain_temp[(j3 * 3 + 0) / (uint)luma_subblock_size_x]
                                     [i * luma_subblock_size_x + (j3 * 3 + 0) % (uint)luma_subblock_size_x];
              res.y = luma_grain_temp[(j3 * 3 + 1) / (uint)luma_subblock_size_x]
                                     [i * luma_subblock_size_x + (j3 * 3 + 1) % (uint)luma_subblock_size_x];
              res.z = luma_grain_temp[(j3 * 3 + 2) / (uint)luma_subblock_size_x]
                                     [i * luma_subblock_size_x + (j3 * 3 + 2) % (uint)luma_subblock_size_x];
              if (((y + i) < data.height) && ((x3 + j3 * 3) < data.width)) {
                dst.Store(dst_luma_plane.y + (y + i) * dst_luma_plane.x + 4 * x3 / 3U + j3 * 4,
                          ((res.z & 0x3ff) << 20) | ((res.y & 0x3ff) << 10) | (res.x & 0x3ff));
              }
            }
          }

          // Mean luma calculation
          if (chroma_subsamp_x) {
            if ((i & 1) == 0) {
              avarage_luma[lid][(i >> 1) * chroma_subblock_size_x + (j >> 1) + 0] = (in_luma.x + in_luma.y + 1) >> 1;
              avarage_luma[lid][(i >> 1) * chroma_subblock_size_x + (j >> 1) + 1] = (in_luma.z + in_luma.w + 1) >> 1;
            }
          } else {
            avarage_luma[lid][i * chroma_subblock_size_x + j + 0] = in_luma.x;
            avarage_luma[lid][i * chroma_subblock_size_x + j + 1] = in_luma.y;
            avarage_luma[lid][i * chroma_subblock_size_x + j + 2] = in_luma.z;
            avarage_luma[lid][i * chroma_subblock_size_x + j + 3] = in_luma.w;
          }

          GroupMemoryBarrierWithGroupSync();

          // Chroma block application
          if (i < chroma_subblock_size_y && j < chroma_subblock_size_x && enable_chroma) {
            {  // cb
              int4 avarage_luma_c;
              avarage_luma_c.x = avarage_luma[lid][i * true_chroma_subblock_size_x + j + 0];
              avarage_luma_c.y = avarage_luma[lid][i * true_chroma_subblock_size_x + j + 1];
              avarage_luma_c.z = avarage_luma[lid][i * true_chroma_subblock_size_x + j + 2];
              avarage_luma_c.w = avarage_luma[lid][i * true_chroma_subblock_size_x + j + 3];

              int2 chroma_plane = data.src_planes[1].xy;
              int4 in_cb;
              if (bit_depth == 8) {
                uint cb_uint = src.Load(chroma_plane.y + ((y >> chroma_subsamp_y) + i) * chroma_plane.x +
                                        (x >> chroma_subsamp_x) + j);
                in_cb.x = (cb_uint >> 0) & 255;
                in_cb.y = (cb_uint >> 8) & 255;
                in_cb.z = (cb_uint >> 16) & 255;
                in_cb.w = (cb_uint >> 24) & 255;
              } else {
                uint2 cb_uint = src.Load2(chroma_plane.y + ((y >> chroma_subsamp_y) + i) * chroma_plane.x +
                                          ((x >> chroma_subsamp_x) + j) * 2);
                in_cb.x = (cb_uint.x >> 0) & 0x03ff;
                in_cb.y = (cb_uint.x >> 16) & 0x03ff;
                in_cb.z = (cb_uint.y >> 0) & 0x03ff;
                in_cb.w = (cb_uint.y >> 16) & 0x03ff;
              }
              int4 cb_to_scale = clamp(((avarage_luma_c * cb_luma_mult + cb_mult * in_cb) >> 6) + cb_offset, 0,
                                       (256 << (bit_depth - 8)) - 1);
              int4 scaled_cb;
              if (bit_depth == 8) {
                scaled_cb.x =
                    data.scaling_lut[cb_to_scale.x].y * (cb_grain_temp[lid][i * true_chroma_subblock_size_x + j + 0]);
                scaled_cb.y =
                    data.scaling_lut[cb_to_scale.y].y * (cb_grain_temp[lid][i * true_chroma_subblock_size_x + j + 1]);
                scaled_cb.z =
                    data.scaling_lut[cb_to_scale.z].y * (cb_grain_temp[lid][i * true_chroma_subblock_size_x + j + 2]);
                scaled_cb.w =
                    data.scaling_lut[cb_to_scale.w].y * (cb_grain_temp[lid][i * true_chroma_subblock_size_x + j + 3]);
              } else {
                scaled_cb.x =
                    scale_LUT_cb(cb_to_scale.x) * (cb_grain_temp[lid][i * true_chroma_subblock_size_x + j + 0]);
                scaled_cb.y =
                    scale_LUT_cb(cb_to_scale.y) * (cb_grain_temp[lid][i * true_chroma_subblock_size_x + j + 1]);
                scaled_cb.z =
                    scale_LUT_cb(cb_to_scale.z) * (cb_grain_temp[lid][i * true_chroma_subblock_size_x + j + 2]);
                scaled_cb.w =
                    scale_LUT_cb(cb_to_scale.w) * (cb_grain_temp[lid][i * true_chroma_subblock_size_x + j + 3]);
              }
              int4 out_cb =
                  clamp(in_cb + ((scaled_cb + rounding_offset) >> data.params.scaling_shift), min_chroma, max_chroma);
              if (!apply_cb) {
                out_cb = in_cb;
              }
              if (data.is_10x3) {
                cb_grain_temp[lid][i * true_chroma_subblock_size_x + j + 0] = out_cb.x;
                cb_grain_temp[lid][i * true_chroma_subblock_size_x + j + 1] = out_cb.y;
                cb_grain_temp[lid][i * true_chroma_subblock_size_x + j + 2] = out_cb.z;
                cb_grain_temp[lid][i * true_chroma_subblock_size_x + j + 3] = out_cb.w;
              } else {
                if ((((y >> chroma_subsamp_y) + i) < (data.height >> chroma_subsamp_y)) &&
                    (((x >> chroma_subsamp_x) + j) < (data.width >> chroma_subsamp_x))) {
                  chroma_plane = data.dst_planes[1].xy;
                  if (bit_depth == 8) {
                    dst.Store(
                        chroma_plane.y + ((y >> chroma_subsamp_y) + i) * chroma_plane.x + (x >> chroma_subsamp_x) + j,
                        out_cb.x | (out_cb.y << 8) | (out_cb.z << 16) | (out_cb.w << 24));
                  } else {
                    dst.Store2(chroma_plane.y + ((y >> chroma_subsamp_y) + i) * chroma_plane.x +
                                   ((x >> chroma_subsamp_x) + j) * 2,
                               uint2((out_cb.x << 0) | (out_cb.y << 16), (out_cb.z << 0) | (out_cb.w << 16)));
                  }
                }
              }
            }
            {  // cr
              int4 avarage_luma_c;
              avarage_luma_c.x = avarage_luma[lid][i * true_chroma_subblock_size_x + j + 0];
              avarage_luma_c.y = avarage_luma[lid][i * true_chroma_subblock_size_x + j + 1];
              avarage_luma_c.z = avarage_luma[lid][i * true_chroma_subblock_size_x + j + 2];
              avarage_luma_c.w = avarage_luma[lid][i * true_chroma_subblock_size_x + j + 3];

              int2 chroma_plane = data.src_planes[2].xy;
              int4 in_cr;
              if (bit_depth == 8) {
                uint cr_uint = src.Load(chroma_plane.y + ((y >> chroma_subsamp_y) + i) * chroma_plane.x +
                                        (x >> chroma_subsamp_x) + j);
                in_cr.x = (cr_uint >> 0) & 255;
                in_cr.y = (cr_uint >> 8) & 255;
                in_cr.z = (cr_uint >> 16) & 255;
                in_cr.w = (cr_uint >> 24) & 255;
              } else {
                uint2 cr_uint = src.Load2(chroma_plane.y + ((y >> chroma_subsamp_y) + i) * chroma_plane.x +
                                          ((x >> chroma_subsamp_x) + j) * 2);
                in_cr.x = (cr_uint.x >> 0) & 0x03ff;
                in_cr.y = (cr_uint.x >> 16) & 0x03ff;
                in_cr.z = (cr_uint.y >> 0) & 0x03ff;
                in_cr.w = (cr_uint.y >> 16) & 0x03ff;
              }

              int4 cr_to_scale = clamp(((avarage_luma_c * cr_luma_mult + cr_mult * in_cr) >> 6) + cr_offset, 0,
                                       (256 << (bit_depth - 8)) - 1);

              int4 scaled_cr;
              if (bit_depth == 8) {
                scaled_cr.x =
                    data.scaling_lut[cr_to_scale.x].z * (cr_grain_temp[lid][i * true_chroma_subblock_size_x + j + 0]);
                scaled_cr.y =
                    data.scaling_lut[cr_to_scale.y].z * (cr_grain_temp[lid][i * true_chroma_subblock_size_x + j + 1]);
                scaled_cr.z =
                    data.scaling_lut[cr_to_scale.z].z * (cr_grain_temp[lid][i * true_chroma_subblock_size_x + j + 2]);
                scaled_cr.w =
                    data.scaling_lut[cr_to_scale.w].z * (cr_grain_temp[lid][i * true_chroma_subblock_size_x + j + 3]);
              } else {
                scaled_cr.x =
                    scale_LUT_cr(cr_to_scale.x) * (cr_grain_temp[lid][i * true_chroma_subblock_size_x + j + 0]);
                scaled_cr.y =
                    scale_LUT_cr(cr_to_scale.y) * (cr_grain_temp[lid][i * true_chroma_subblock_size_x + j + 1]);
                scaled_cr.z =
                    scale_LUT_cr(cr_to_scale.z) * (cr_grain_temp[lid][i * true_chroma_subblock_size_x + j + 2]);
                scaled_cr.w =
                    scale_LUT_cr(cr_to_scale.w) * (cr_grain_temp[lid][i * true_chroma_subblock_size_x + j + 3]);
              }

              int4 out_cr =
                  clamp(in_cr + ((scaled_cr + rounding_offset) >> data.params.scaling_shift), min_chroma, max_chroma);
              if (!apply_cr) {
                out_cr = in_cr;
              }
              if (data.is_10x3) {
                cr_grain_temp[lid][i * true_chroma_subblock_size_x + j + 0] = out_cr.x;
                cr_grain_temp[lid][i * true_chroma_subblock_size_x + j + 1] = out_cr.y;
                cr_grain_temp[lid][i * true_chroma_subblock_size_x + j + 2] = out_cr.z;
                cr_grain_temp[lid][i * true_chroma_subblock_size_x + j + 3] = out_cr.w;
              } else {
                if ((((y >> chroma_subsamp_y) + i) < (data.height >> chroma_subsamp_y)) &&
                    (((x >> chroma_subsamp_x) + j) < (data.width >> chroma_subsamp_x))) {
                  chroma_plane = data.dst_planes[2].xy;
                  if (bit_depth == 8) {
                    dst.Store(
                        chroma_plane.y + ((y >> chroma_subsamp_y) + i) * chroma_plane.x + (x >> chroma_subsamp_x) + j,
                        out_cr.x | (out_cr.y << 8) | (out_cr.z << 16) | (out_cr.w << 24));
                  } else {
                    dst.Store2(chroma_plane.y + ((y >> chroma_subsamp_y) + i) * chroma_plane.x +
                                   ((x >> chroma_subsamp_x) + j) * 2,
                               uint2((out_cr.x << 0) | (out_cr.y << 16), (out_cr.z << 0) | (out_cr.w << 16)));
                  }
                }
              }
            }
          }

          GroupMemoryBarrierWithGroupSync();

          if (data.is_10x3) {
            int x3 = Gid.x * 3 * true_chroma_subblock_size_x;
            if (GTid.y < true_chroma_subblock_size_y) {
              i = GTid.y;
              int2 chroma_plane = data.dst_planes[1].xy;
              for (int j3 = GTid.z * (true_chroma_subblock_size_x / 4) + GTid.x; j3 < true_chroma_subblock_size_x;
                   j3 += (true_chroma_subblock_size_x / 4)) {
                uint3 res;
                res.x =
                    cb_grain_temp[(j3 * 3 + 0) / (uint)true_chroma_subblock_size_x]
                                 [i * true_chroma_subblock_size_x + (j3 * 3 + 0) % (uint)true_chroma_subblock_size_x];
                res.y =
                    cb_grain_temp[(j3 * 3 + 1) / (uint)true_chroma_subblock_size_x]
                                 [i * true_chroma_subblock_size_x + (j3 * 3 + 1) % (uint)true_chroma_subblock_size_x];
                res.z =
                    cb_grain_temp[(j3 * 3 + 2) / (uint)true_chroma_subblock_size_x]
                                 [i * true_chroma_subblock_size_x + (j3 * 3 + 2) % (uint)true_chroma_subblock_size_x];
                if ((((y >> 1) + i) < (data.height >> 1)) && ((x3 + j3 * 3) < (data.width >> 1))) {
                  dst.Store(chroma_plane.y + ((y >> 1) + i) * chroma_plane.x + 4 * x3 / 3U + j3 * 4,
                            ((res.z & 0x3ff) << 20) | ((res.y & 0x3ff) << 10) | (res.x & 0x3ff));
                }
              }
            } else {
              i = GTid.y - true_chroma_subblock_size_y;
              int2 chroma_plane = data.dst_planes[2].xy;
              for (int j3 = GTid.z * (true_chroma_subblock_size_x / 4) + GTid.x; j3 < true_chroma_subblock_size_x;
                   j3 += (true_chroma_subblock_size_x / 4)) {
                uint3 res;
                res.x =
                    cr_grain_temp[(j3 * 3 + 0) / (uint)true_chroma_subblock_size_x]
                                 [i * true_chroma_subblock_size_x + (j3 * 3 + 0) % (uint)true_chroma_subblock_size_x];
                res.y =
                    cr_grain_temp[(j3 * 3 + 1) / (uint)chroma_subblock_size_x]
                                 [i * true_chroma_subblock_size_x + (j3 * 3 + 1) % (uint)true_chroma_subblock_size_x];
                res.z =
                    cr_grain_temp[(j3 * 3 + 2) / (uint)chroma_subblock_size_x]
                                 [i * true_chroma_subblock_size_x + (j3 * 3 + 2) % (uint)true_chroma_subblock_size_x];
                if ((((y >> 1) + i) < (data.height >> 1)) && ((x3 + j3 * 3) < (data.width >> 1))) {
                  dst.Store(chroma_plane.y + (y / 2U + i) * chroma_plane.x + 4 * x3 / 3U + j3 * 4,
                            ((res.z & 0x3ff) << 20) | ((res.y & 0x3ff) << 10) | (res.x & 0x3ff));
                }
              }
            }
          }
        }
      }
    }
  }
}
