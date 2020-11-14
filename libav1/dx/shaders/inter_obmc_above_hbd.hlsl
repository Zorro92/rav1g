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
#define OutputShift 11
#define OffsetBits 21
#define OutputRoundAdd ((1 << (OutputShift - 1)) + (1 << OffsetBits))
#define OutputSub ((1 << (OffsetBits - OutputShift)) + (1 << (OffsetBits - OutputShift - 1)))
#define PixelMax 1023

void store_vblend(RWByteAddressBuffer buf, uint addr, int4 v, int mask) {
  uint2 src0 = buf.Load2(addr);
  v.x = (((src0.x >> 0) & 1023) * mask + v.x * (64 - mask) + 32) >> 6;
  v.y = (((src0.x >> 16) & 1023) * mask + v.y * (64 - mask) + 32) >> 6;
  v.z = (((src0.y >> 0) & 1023) * mask + v.z * (64 - mask) + 32) >> 6;
  v.w = (((src0.y >> 16) & 1023) * mask + v.w * (64 - mask) + 32) >> 6;
  buf.Store2(addr, uint2(v.x | (v.y << 16), v.z | (v.w << 16)));
}

[numthreads(64, 1, 1)] void main(uint3 thread
                                 : SV_DispatchThreadID) {
  if (thread.x >= cb_wi_count) return;

  const int w_log = cb_width_log2;
  const int h_log = cb_height_log2;
  const int subblock = thread.x & ((1 << (w_log + h_log)) - 1);

  uint4 block = pred_blocks.Load4((cb_pass_offset + (thread.x >> (w_log + h_log))) * 16);

  int x = SubblockW * ((block.x & 0xffff) + (subblock & ((1 << w_log) - 1)));
  int y = SubblockH * ((block.x >> 16) + (subblock >> w_log));

  const int plane = block.y & 3;
  const int2 dims = cb_dims[plane > 0].xy;

  int mv = block.z;
  int mvx = x + ((mv) >> (16 + SUBPEL_BITS)) - 3;
  int mvy = y + ((mv << 16) >> (16 + SUBPEL_BITS)) - 3;
  mvx = clamp(mvx, -11, dims.x) << 1;

  const int filter_h = (((block.y >> 5) & 15) << 4) + ((mv >> 16) & SUBPEL_MASK);
  const int filter_v = (((block.y >> 9) & 15) << 4) + (mv & SUBPEL_MASK);

  const int refplane = ((block.y >> 2) & 7) * 3 + plane;
  int ref_offset = cb_refplanes[refplane].y;
  int ref_stride = cb_refplanes[refplane].x;

  int4 kernel_h0 = cb_kernels[filter_h][0];
  int4 kernel_h1 = cb_kernels[filter_h][1];

  int4 output[4] = {{0, 0, 0, 0}, {0, 0, 0, 0}, {0, 0, 0, 0}, {0, 0, 0, 0}};

  int4 l;
  l = filter_line_hbd(dst_frame, ref_offset + mvx + ref_stride * clamp(mvy + 0, 0, dims.y), kernel_h0, kernel_h1);
  output[0] += l * cb_kernels[filter_v][0].x;

  l = filter_line_hbd(dst_frame, ref_offset + mvx + ref_stride * clamp(mvy + 1, 0, dims.y), kernel_h0, kernel_h1);
  output[1] += l * cb_kernels[filter_v][0].x;
  output[0] += l * cb_kernels[filter_v][0].y;

  l = filter_line_hbd(dst_frame, ref_offset + mvx + ref_stride * clamp(mvy + 2, 0, dims.y), kernel_h0, kernel_h1);
  output[2] += l * cb_kernels[filter_v][0].x;
  output[1] += l * cb_kernels[filter_v][0].y;
  output[0] += l * cb_kernels[filter_v][0].z;

  //
  l = filter_line_hbd(dst_frame, ref_offset + mvx + ref_stride * clamp(mvy + 3, 0, dims.y), kernel_h0, kernel_h1);
  output[3] += l * cb_kernels[filter_v][0].x;
  output[2] += l * cb_kernels[filter_v][0].y;
  output[1] += l * cb_kernels[filter_v][0].z;
  output[0] += l * cb_kernels[filter_v][0].w;
  l = filter_line_hbd(dst_frame, ref_offset + mvx + ref_stride * clamp(mvy + 4, 0, dims.y), kernel_h0, kernel_h1);
  output[3] += l * cb_kernels[filter_v][0].y;
  output[2] += l * cb_kernels[filter_v][0].z;
  output[1] += l * cb_kernels[filter_v][0].w;
  output[0] += l * cb_kernels[filter_v][1].x;
  l = filter_line_hbd(dst_frame, ref_offset + mvx + ref_stride * clamp(mvy + 5, 0, dims.y), kernel_h0, kernel_h1);
  output[3] += l * cb_kernels[filter_v][0].z;
  output[2] += l * cb_kernels[filter_v][0].w;
  output[1] += l * cb_kernels[filter_v][1].x;
  output[0] += l * cb_kernels[filter_v][1].y;
  l = filter_line_hbd(dst_frame, ref_offset + mvx + ref_stride * clamp(mvy + 6, 0, dims.y), kernel_h0, kernel_h1);
  output[3] += l * cb_kernels[filter_v][0].w;
  output[2] += l * cb_kernels[filter_v][1].x;
  output[1] += l * cb_kernels[filter_v][1].y;
  output[0] += l * cb_kernels[filter_v][1].z;
  l = filter_line_hbd(dst_frame, ref_offset + mvx + ref_stride * clamp(mvy + 7, 0, dims.y), kernel_h0, kernel_h1);
  output[3] += l * cb_kernels[filter_v][1].x;
  output[2] += l * cb_kernels[filter_v][1].y;
  output[1] += l * cb_kernels[filter_v][1].z;
  output[0] += l * cb_kernels[filter_v][1].w;
  //

  l = filter_line_hbd(dst_frame, ref_offset + mvx + ref_stride * clamp(mvy + 8, 0, dims.y), kernel_h0, kernel_h1);
  output[3] += l * cb_kernels[filter_v][1].y;
  output[2] += l * cb_kernels[filter_v][1].z;
  output[1] += l * cb_kernels[filter_v][1].w;

  l = filter_line_hbd(dst_frame, ref_offset + mvx + ref_stride * clamp(mvy + 9, 0, dims.y), kernel_h0, kernel_h1);
  output[3] += l * cb_kernels[filter_v][1].z;
  output[2] += l * cb_kernels[filter_v][1].w;

  l = filter_line_hbd(dst_frame, ref_offset + mvx + ref_stride * clamp(mvy + 10, 0, dims.y), kernel_h0, kernel_h1);
  output[3] += l * cb_kernels[filter_v][1].w;

  const int output_stride = cb_planes[plane].x;
  const int output_offset = cb_planes[plane].y + (x << 1) + y * output_stride;

  for (int i = 0; i < 4; ++i) {
    output[i].x = clamp((int)(((output[i].x + OutputRoundAdd) >> OutputShift) - OutputSub), 0, PixelMax);
    output[i].y = clamp((int)(((output[i].y + OutputRoundAdd) >> OutputShift) - OutputSub), 0, PixelMax);
    output[i].z = clamp((int)(((output[i].z + OutputRoundAdd) >> OutputShift) - OutputSub), 0, PixelMax);
    output[i].w = clamp((int)(((output[i].w + OutputRoundAdd) >> OutputShift) - OutputSub), 0, PixelMax);
  }

  int4 m = cb_obmc_mask[(block.y >> 17) + (subblock >> w_log)];

  store_vblend(dst_frame, output_offset + 0 * output_stride, output[0], m.x);
  store_vblend(dst_frame, output_offset + 1 * output_stride, output[1], m.y);
  store_vblend(dst_frame, output_offset + 2 * output_stride, output[2], m.z);
  store_vblend(dst_frame, output_offset + 3 * output_stride, output[3], m.w);
}
