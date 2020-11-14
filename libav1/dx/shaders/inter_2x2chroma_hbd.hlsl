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

#define SubblockW 2
#define SubblockH 2
#define OutputShift 11
#define OffsetBits 21
#define OutputRoundAdd ((1 << (OutputShift - 1)) + (1 << OffsetBits))
#define OutputSub ((1 << (OffsetBits - OutputShift)) + (1 << (OffsetBits - OutputShift - 1)))
#define PixelMax 1023

groupshared uint2 mem[64];

[numthreads(64, 1, 1)] void main(uint3 thread
                                 : SV_DispatchThreadID) {
  if (thread.x >= cb_wi_count) return;

  uint4 block = pred_blocks.Load4((cb_pass_offset + thread.x) * 16);

  int x = SubblockW * (block.x & 0xffff);
  int y = SubblockH * (block.x >> 16);

  const int2 dims = cb_dims[1].xy;
  const int plane = block.y & 3;
  int noskip = block.y & NoSkipFlag;

  int mv = block.z;
  int mvx = x + ((mv) >> (16 + SUBPEL_BITS)) - 3;
  int mvy = y + ((mv << 16) >> (16 + SUBPEL_BITS)) - 3;
  mvx = clamp(mvx, -11, dims.x) << 1;

  int filter_h = (((block.y >> 5) & 15) << 4) + ((mv >> 16) & SUBPEL_MASK);
  int filter_v = (((block.y >> 9) & 15) << 4) + (mv & SUBPEL_MASK);

  int refplane = ((block.y >> 2) & 7) * 3 + plane;
  int ref_offset = cb_refplanes[refplane].y;
  int ref_stride = cb_refplanes[refplane].x;

  int4 kernel_h0 = cb_kernels[filter_h][0];
  int4 kernel_h1 = cb_kernels[filter_h][1];

  int4 tmp = {0, 0, 0, 0};

  int2 l;
  l = filter_line2_hbd(dst_frame, ref_offset + mvx + ref_stride * clamp(mvy + 0, 0, dims.y), kernel_h0, kernel_h1);
  tmp.xy += l * cb_kernels[filter_v][0].x;
  l = filter_line2_hbd(dst_frame, ref_offset + mvx + ref_stride * clamp(mvy + 1, 0, dims.y), kernel_h0, kernel_h1);
  tmp.zw += l * cb_kernels[filter_v][0].x;
  tmp.xy += l * cb_kernels[filter_v][0].y;
  l = filter_line2_hbd(dst_frame, ref_offset + mvx + ref_stride * clamp(mvy + 2, 0, dims.y), kernel_h0, kernel_h1);
  tmp.zw += l * cb_kernels[filter_v][0].y;
  tmp.xy += l * cb_kernels[filter_v][0].z;
  l = filter_line2_hbd(dst_frame, ref_offset + mvx + ref_stride * clamp(mvy + 3, 0, dims.y), kernel_h0, kernel_h1);
  tmp.zw += l * cb_kernels[filter_v][0].z;
  tmp.xy += l * cb_kernels[filter_v][0].w;
  l = filter_line2_hbd(dst_frame, ref_offset + mvx + ref_stride * clamp(mvy + 4, 0, dims.y), kernel_h0, kernel_h1);
  tmp.zw += l * cb_kernels[filter_v][0].w;
  tmp.xy += l * cb_kernels[filter_v][1].x;
  l = filter_line2_hbd(dst_frame, ref_offset + mvx + ref_stride * clamp(mvy + 5, 0, dims.y), kernel_h0, kernel_h1);
  tmp.zw += l * cb_kernels[filter_v][1].x;
  tmp.xy += l * cb_kernels[filter_v][1].y;
  l = filter_line2_hbd(dst_frame, ref_offset + mvx + ref_stride * clamp(mvy + 6, 0, dims.y), kernel_h0, kernel_h1);
  tmp.zw += l * cb_kernels[filter_v][1].y;
  tmp.xy += l * cb_kernels[filter_v][1].z;
  l = filter_line2_hbd(dst_frame, ref_offset + mvx + ref_stride * clamp(mvy + 7, 0, dims.y), kernel_h0, kernel_h1);
  tmp.zw += l * cb_kernels[filter_v][1].z;
  tmp.xy += l * cb_kernels[filter_v][1].w;
  l = filter_line2_hbd(dst_frame, ref_offset + mvx + ref_stride * clamp(mvy + 8, 0, dims.y), kernel_h0, kernel_h1);
  tmp.zw += l * cb_kernels[filter_v][1].w;
  tmp.x = clamp((int)(((tmp.x + OutputRoundAdd) >> OutputShift) - OutputSub), 0, PixelMax);
  tmp.y = clamp((int)(((tmp.y + OutputRoundAdd) >> OutputShift) - OutputSub), 0, PixelMax);
  tmp.z = clamp((int)(((tmp.z + OutputRoundAdd) >> OutputShift) - OutputSub), 0, PixelMax);
  tmp.w = clamp((int)(((tmp.w + OutputRoundAdd) >> OutputShift) - OutputSub), 0, PixelMax);

  /// add residuals here
  x <<= 1;
  const int res_stride = cb_planes[plane].z;
  const int res_offset = cb_planes[plane].w + x + y * res_stride;
  if (noskip) {
    int r0 = residuals.Load(res_offset);
    int r1 = residuals.Load(res_offset + res_stride);
    tmp.x += (r0 << 16) >> 16;
    tmp.y += r0 >> 16;
    tmp.z += (r1 << 16) >> 16;
    tmp.w += r1 >> 16;
    tmp = clamp(tmp, 0, PixelMax);
  }

  const int output_stride = cb_planes[plane].x;
  const int output_offset = cb_planes[plane].y + x + y * output_stride;
  dst_frame.Store(output_offset, tmp.x | (tmp.y << 16));
  dst_frame.Store(output_offset + output_stride, tmp.z | (tmp.w << 16));
}
