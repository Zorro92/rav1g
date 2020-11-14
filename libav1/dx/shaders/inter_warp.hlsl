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

#include "inter_common.h"

#define SubblockW 4
#define SubblockH 4
#define VertBits 11
#define VertSumAdd ((1 << 19) + (1 << (VertBits - 1)))
#define VertSub ((1 << (8 - 1)) + (1 << 8))
#define FilterSize 32
#define BlockSize 48

groupshared int4 mem[11 * 64];

[numthreads(64, 1, 1)] void main(uint3 thread
                                 : SV_DispatchThreadID) {
  if (thread.x >= cb_wi_count) return;

  const int w_log = cb_width_log2;
  const int h_log = cb_height_log2;
  const int subblock = thread.x & ((1 << (w_log + h_log)) - 1);

  const int offset = (cb_pass_offset + (thread.x >> (w_log + h_log))) * BlockSize;
  uint4 info0 = warp_blocks.Load4(offset);
  int4 info1 = warp_blocks.Load4(offset + 16);
  int4 info2 = warp_blocks.Load4(offset + 32);
  // struct
  //{
  //    uint pos;
  //    uint flags;
  //    int mat[6];
  //    int alpha;
  //    int beta;
  //    int delta;
  //    int gamma;
  //};

  int x = SubblockW * ((info0.x & 0xffff) + (subblock & ((1 << w_log) - 1)));
  int y = SubblockH * ((info0.x >> 16) + (subblock >> w_log));

  const int plane = info0.y & 3;
  const int subsampling = plane > 0;
  const int2 dims = cb_dims[subsampling].xy;
  const int noskip = info0.y & NoSkipFlag;

  const int src_x = ((x & (~7)) + 4) << subsampling;
  const int src_y = ((y & (~7)) + 4) << subsampling;
  const int dst_x = info1.x * src_x + info1.y * src_y + (int)info0.z;
  const int dst_y = info1.z * src_x + info1.w * src_y + (int)info0.w;
  const int x4 = dst_x >> subsampling;
  const int y4 = dst_y >> subsampling;

  int ix4 = clamp((x4 >> WarpPrecBits) - 7 + (x & 7), -11, dims.x);
  int iy4 = (y4 >> WarpPrecBits) - 7 + (y & 7);

  int sx4 = x4 & ((1 << WarpPrecBits) - 1);
  int sy4 = y4 & ((1 << WarpPrecBits) - 1);

  sx4 += info2.x * (-4) + info2.y * (-4);
  sy4 += info2.w * (-4) + info2.z * (-4);

  sx4 &= ~((1 << WarpReduceBits) - 1);
  sy4 &= ~((1 << WarpReduceBits) - 1);

  sx4 += info2.y * ((y & 7) - 3) + info2.x * (x & 7);
  sy4 += info2.z * (y & 7) + info2.w * (x & 7);

  int refplane = ((info0.y >> 2) & 7) * 3 + plane;
  int ref_offset = cb_refplanes[refplane].y + ix4;
  int ref_stride = cb_refplanes[refplane].x;

  int local_ofs = (thread.x & 63) * 11;
  mem[local_ofs + 0] = filter_line_warp(dst_frame, ref_offset + ref_stride * clamp(iy4 + 0, 0, dims.y), warp_filter,
                                        sx4 + info2.y * 0, info2.x);
  mem[local_ofs + 1] = filter_line_warp(dst_frame, ref_offset + ref_stride * clamp(iy4 + 1, 0, dims.y), warp_filter,
                                        sx4 + info2.y * 1, info2.x);
  mem[local_ofs + 2] = filter_line_warp(dst_frame, ref_offset + ref_stride * clamp(iy4 + 2, 0, dims.y), warp_filter,
                                        sx4 + info2.y * 2, info2.x);
  mem[local_ofs + 3] = filter_line_warp(dst_frame, ref_offset + ref_stride * clamp(iy4 + 3, 0, dims.y), warp_filter,
                                        sx4 + info2.y * 3, info2.x);
  mem[local_ofs + 4] = filter_line_warp(dst_frame, ref_offset + ref_stride * clamp(iy4 + 4, 0, dims.y), warp_filter,
                                        sx4 + info2.y * 4, info2.x);
  mem[local_ofs + 5] = filter_line_warp(dst_frame, ref_offset + ref_stride * clamp(iy4 + 5, 0, dims.y), warp_filter,
                                        sx4 + info2.y * 5, info2.x);
  mem[local_ofs + 6] = filter_line_warp(dst_frame, ref_offset + ref_stride * clamp(iy4 + 6, 0, dims.y), warp_filter,
                                        sx4 + info2.y * 6, info2.x);
  mem[local_ofs + 7] = filter_line_warp(dst_frame, ref_offset + ref_stride * clamp(iy4 + 7, 0, dims.y), warp_filter,
                                        sx4 + info2.y * 7, info2.x);
  mem[local_ofs + 8] = filter_line_warp(dst_frame, ref_offset + ref_stride * clamp(iy4 + 8, 0, dims.y), warp_filter,
                                        sx4 + info2.y * 8, info2.x);
  mem[local_ofs + 9] = filter_line_warp(dst_frame, ref_offset + ref_stride * clamp(iy4 + 9, 0, dims.y), warp_filter,
                                        sx4 + info2.y * 9, info2.x);
  mem[local_ofs + 10] = filter_line_warp(dst_frame, ref_offset + ref_stride * clamp(iy4 + 10, 0, dims.y), warp_filter,
                                         sx4 + info2.y * 10, info2.x);

  const int output_stride = cb_planes[plane].x;
  const int output_offset = cb_planes[plane].y + x + y * output_stride;
  const int res_stride = cb_planes[plane].z;
  const int res_offset = cb_planes[plane].w + (x << 1) + y * res_stride;
  for (int l = 0; l < 4; ++l) {
    int4 output;
    int sy = sy4 + l * info2.z;

    int filter_addr;
    int4 filter0, filter1;

    filter_addr = FilterSize * (((sy + 0 * info2.w + WarpFiltRoundAdd) >> WarpFiltRoundBits) + WarpFiltOffset);
    filter0 = warp_filter.Load4(filter_addr);
    filter1 = warp_filter.Load4(filter_addr + 16);
    output.x = mem[local_ofs + l + 0].x * filter0.x + mem[local_ofs + l + 1].x * filter0.y +
               mem[local_ofs + l + 2].x * filter0.z + mem[local_ofs + l + 3].x * filter0.w +
               mem[local_ofs + l + 4].x * filter1.x + mem[local_ofs + l + 5].x * filter1.y +
               mem[local_ofs + l + 6].x * filter1.z + mem[local_ofs + l + 7].x * filter1.w;

    filter_addr = FilterSize * (((sy + 1 * info2.w + WarpFiltRoundAdd) >> WarpFiltRoundBits) + WarpFiltOffset);
    filter0 = warp_filter.Load4(filter_addr);
    filter1 = warp_filter.Load4(filter_addr + 16);
    output.y = mem[local_ofs + l + 0].y * filter0.x + mem[local_ofs + l + 1].y * filter0.y +
               mem[local_ofs + l + 2].y * filter0.z + mem[local_ofs + l + 3].y * filter0.w +
               mem[local_ofs + l + 4].y * filter1.x + mem[local_ofs + l + 5].y * filter1.y +
               mem[local_ofs + l + 6].y * filter1.z + mem[local_ofs + l + 7].y * filter1.w;

    filter_addr = FilterSize * (((sy + 2 * info2.w + WarpFiltRoundAdd) >> WarpFiltRoundBits) + WarpFiltOffset);
    filter0 = warp_filter.Load4(filter_addr);
    filter1 = warp_filter.Load4(filter_addr + 16);
    output.z = mem[local_ofs + l + 0].z * filter0.x + mem[local_ofs + l + 1].z * filter0.y +
               mem[local_ofs + l + 2].z * filter0.z + mem[local_ofs + l + 3].z * filter0.w +
               mem[local_ofs + l + 4].z * filter1.x + mem[local_ofs + l + 5].z * filter1.y +
               mem[local_ofs + l + 6].z * filter1.z + mem[local_ofs + l + 7].z * filter1.w;

    filter_addr = FilterSize * (((sy + 3 * info2.w + WarpFiltRoundAdd) >> WarpFiltRoundBits) + WarpFiltOffset);
    filter0 = warp_filter.Load4(filter_addr);
    filter1 = warp_filter.Load4(filter_addr + 16);
    output.w = mem[local_ofs + l + 0].w * filter0.x + mem[local_ofs + l + 1].w * filter0.y +
               mem[local_ofs + l + 2].w * filter0.z + mem[local_ofs + l + 3].w * filter0.w +
               mem[local_ofs + l + 4].w * filter1.x + mem[local_ofs + l + 5].w * filter1.y +
               mem[local_ofs + l + 6].w * filter1.z + mem[local_ofs + l + 7].w * filter1.w;

    output.x = clamp((int)(((output.x + VertSumAdd) >> VertBits) - VertSub), 0, 255);
    output.y = clamp((int)(((output.y + VertSumAdd) >> VertBits) - VertSub), 0, 255);
    output.z = clamp((int)(((output.z + VertSumAdd) >> VertBits) - VertSub), 0, 255);
    output.w = clamp((int)(((output.w + VertSumAdd) >> VertBits) - VertSub), 0, 255);

    if (noskip) {
      int2 r = (int2)residuals.Load2(res_offset + l * res_stride);
      output.x += (r.x << 16) >> 16;
      output.y += r.x >> 16;
      output.z += (r.y << 16) >> 16;
      output.w += r.y >> 16;
      output = clamp(output, 0, 255);
    }

    dst_frame.Store(output_offset + l * output_stride,
                    output.x | (output.y << 8) | (output.z << 16) | (output.w << 24));
  }
}
