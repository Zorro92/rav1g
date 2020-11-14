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
#define OutputShift 7
#define OffsetBits 21
#define SumAdd (1 << OffsetBits)
#define OutputRoundAdd ((1 << (OutputShift - 1)) + (1 << OffsetBits))
#define OutputSub ((1 << (OffsetBits - OutputShift)) + (1 << (OffsetBits - OutputShift - 1)))
#define RoundFinal 4
#define DistBits 4
#define PixelMax 1023

int blend(int src0, int src1, int coef0, int coef1) {
  src0 = (src0 + OutputRoundAdd) >> OutputShift;
  src1 = (src1 + OutputRoundAdd) >> OutputShift;
  int result = (src0 * coef0 + src1 * coef1) >> DistBits;
  result = (result - OutputSub + (1 << (RoundFinal - 1))) >> RoundFinal;
  return clamp(result, 0, PixelMax);
}

groupshared int2 mem[64];

[numthreads(64, 1, 1)] void main(uint3 thread
                                 : SV_DispatchThreadID) {
  if (thread.x >= cb_wi_count) return;

  uint4 block = pred_blocks.Load4((cb_pass_offset + thread.x) * 16);

  int x = SubblockW * (block.x & 0xffff);
  int y = SubblockH * (block.x >> 16);

  const int2 dims = cb_dims[1].xy;
  const int plane = block.y & 3;
  const int noskip = block.y & NoSkipFlag;

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
  int4 kernel_v0 = cb_kernels[filter_v][0];
  int4 kernel_v1 = cb_kernels[filter_v][1];

  int4 output0 = {0, 0, 0, 0};
  int2 l;
  l = filter_line2_hbd(dst_frame, ref_offset + mvx + ref_stride * clamp(mvy + 0, 0, dims.y), kernel_h0, kernel_h1);
  output0.xy += l * kernel_v0.x;
  l = filter_line2_hbd(dst_frame, ref_offset + mvx + ref_stride * clamp(mvy + 1, 0, dims.y), kernel_h0, kernel_h1);
  output0.zw += l * kernel_v0.x;
  output0.xy += l * kernel_v0.y;
  l = filter_line2_hbd(dst_frame, ref_offset + mvx + ref_stride * clamp(mvy + 2, 0, dims.y), kernel_h0, kernel_h1);
  output0.zw += l * kernel_v0.y;
  output0.xy += l * kernel_v0.z;
  l = filter_line2_hbd(dst_frame, ref_offset + mvx + ref_stride * clamp(mvy + 3, 0, dims.y), kernel_h0, kernel_h1);
  output0.zw += l * kernel_v0.z;
  output0.xy += l * kernel_v0.w;
  l = filter_line2_hbd(dst_frame, ref_offset + mvx + ref_stride * clamp(mvy + 4, 0, dims.y), kernel_h0, kernel_h1);
  output0.zw += l * kernel_v0.w;
  output0.xy += l * kernel_v1.x;
  l = filter_line2_hbd(dst_frame, ref_offset + mvx + ref_stride * clamp(mvy + 5, 0, dims.y), kernel_h0, kernel_h1);
  output0.zw += l * kernel_v1.x;
  output0.xy += l * kernel_v1.y;
  l = filter_line2_hbd(dst_frame, ref_offset + mvx + ref_stride * clamp(mvy + 6, 0, dims.y), kernel_h0, kernel_h1);
  output0.zw += l * kernel_v1.y;
  output0.xy += l * kernel_v1.z;
  l = filter_line2_hbd(dst_frame, ref_offset + mvx + ref_stride * clamp(mvy + 7, 0, dims.y), kernel_h0, kernel_h1);
  output0.zw += l * kernel_v1.z;
  output0.xy += l * kernel_v1.w;
  l = filter_line2_hbd(dst_frame, ref_offset + mvx + ref_stride * clamp(mvy + 8, 0, dims.y), kernel_h0, kernel_h1);
  output0.zw += l * kernel_v1.w;

  mv = block.w;
  mvx = x + ((mv) >> (16 + SUBPEL_BITS)) - 3;
  mvy = y + ((mv << 16) >> (16 + SUBPEL_BITS)) - 3;
  mvx = clamp(mvx, -11, dims.x) << 1;
  filter_h = (((block.y >> 5) & 15) << 4) + ((mv >> 16) & SUBPEL_MASK);
  filter_v = (((block.y >> 9) & 15) << 4) + (mv & SUBPEL_MASK);

  refplane = ((block.y >> 14) & 7) * 3 + plane;
  ref_offset = cb_refplanes[refplane].y;
  ref_stride = cb_refplanes[refplane].x;

  kernel_h0 = cb_kernels[filter_h][0];
  kernel_h1 = cb_kernels[filter_h][1];
  kernel_v0 = cb_kernels[filter_v][0];
  kernel_v1 = cb_kernels[filter_v][1];

  int4 output1 = {0, 0, 0, 0};
  l = filter_line2_hbd(dst_frame, ref_offset + mvx + ref_stride * clamp(mvy + 0, 0, dims.y), kernel_h0, kernel_h1);
  output1.xy += l * kernel_v0.x;
  l = filter_line2_hbd(dst_frame, ref_offset + mvx + ref_stride * clamp(mvy + 1, 0, dims.y), kernel_h0, kernel_h1);
  output1.zw += l * kernel_v0.x;
  output1.xy += l * kernel_v0.y;
  l = filter_line2_hbd(dst_frame, ref_offset + mvx + ref_stride * clamp(mvy + 2, 0, dims.y), kernel_h0, kernel_h1);
  output1.zw += l * kernel_v0.y;
  output1.xy += l * kernel_v0.z;
  l = filter_line2_hbd(dst_frame, ref_offset + mvx + ref_stride * clamp(mvy + 3, 0, dims.y), kernel_h0, kernel_h1);
  output1.zw += l * kernel_v0.z;
  output1.xy += l * kernel_v0.w;
  l = filter_line2_hbd(dst_frame, ref_offset + mvx + ref_stride * clamp(mvy + 4, 0, dims.y), kernel_h0, kernel_h1);
  output1.zw += l * kernel_v0.w;
  output1.xy += l * kernel_v1.x;
  l = filter_line2_hbd(dst_frame, ref_offset + mvx + ref_stride * clamp(mvy + 5, 0, dims.y), kernel_h0, kernel_h1);
  output1.zw += l * kernel_v1.x;
  output1.xy += l * kernel_v1.y;
  l = filter_line2_hbd(dst_frame, ref_offset + mvx + ref_stride * clamp(mvy + 6, 0, dims.y), kernel_h0, kernel_h1);
  output1.zw += l * kernel_v1.y;
  output1.xy += l * kernel_v1.z;
  l = filter_line2_hbd(dst_frame, ref_offset + mvx + ref_stride * clamp(mvy + 7, 0, dims.y), kernel_h0, kernel_h1);
  output1.zw += l * kernel_v1.z;
  output1.xy += l * kernel_v1.w;
  l = filter_line2_hbd(dst_frame, ref_offset + mvx + ref_stride * clamp(mvy + 8, 0, dims.y), kernel_h0, kernel_h1);
  output1.zw += l * kernel_v1.w;

  int coef0 = (block.y >> 17) & 15;
  int coef1 = (block.y >> 21) & 15;

  int4 out4;
  out4.x = blend(output0.x, output1.x, coef0, coef1);
  out4.y = blend(output0.y, output1.y, coef0, coef1);
  out4.z = blend(output0.z, output1.z, coef0, coef1);
  out4.w = blend(output0.w, output1.w, coef0, coef1);

  x <<= 1;
  if (noskip) {
    const int res_stride = cb_planes[plane].z;
    const int res_offset = cb_planes[plane].w + x + y * res_stride;
    int r0 = residuals.Load(res_offset);
    int r1 = residuals.Load(res_offset + res_stride);
    out4.x += (r0 << 16) >> 16;
    out4.y += r0 >> 16;
    out4.z += (r1 << 16) >> 16;
    out4.w += r1 >> 16;
    out4 = clamp(out4, 0, PixelMax);
  }

  const int output_stride = cb_planes[plane].x;
  const int output_offset = cb_planes[plane].y + x + y * output_stride;
  dst_frame.Store(output_offset, out4.x | (out4.y << 16));
  dst_frame.Store(output_offset + output_stride, out4.z | (out4.w << 16));
}
