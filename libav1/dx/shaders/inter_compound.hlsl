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
#define SubblockH 2
#define OutputShift 7
#define OutputRoundAdd (1 << (OutputShift - 1))
#define OffsetBits 19
#define SumAdd (1 << OffsetBits)
#define OutputSub ((1 << (OffsetBits - OutputShift)) + (1 << (OffsetBits - OutputShift - 1)))
#define RoundFinal 4
#define DistBits 4
#define SUM1 1 << OffsetBits

int blend(int src0, int src1, int coef0, int coef1) {
  src0 = (src0 + OutputRoundAdd) >> OutputShift;
  src1 = (src1 + OutputRoundAdd) >> OutputShift;
  int result = (src0 * coef0 + src1 * coef1) >> DistBits;
  result = (result - OutputSub + (1 << (RoundFinal - 1))) >> RoundFinal;
  return clamp(result, 0, 255);
}

[numthreads(64, 1, 1)] void main(uint3 thread
                                 : SV_DispatchThreadID) {
  if (thread.x >= cb_wi_count) return;

  const int w_log = cb_width_log2;
  const int h_log = cb_height_log2;
  const int subblock = thread.x & ((1 << (w_log + h_log)) - 1);
  const int block_index = cb_pass_offset + (thread.x >> (w_log + h_log));
  uint4 block = pred_blocks.Load4(block_index * 16);
  // block.x - pos xy
  // block.y - flags:
  //        2    plane
  //        3    ref
  //        4    filter_x
  //        4    filter_y
  //        1    skip
  //        3    ref1
  //
  int x = SubblockW * ((block.x & 0xffff) + (subblock & ((1 << w_log) - 1)));
  int y = SubblockH * (((block.x >> 16) << 1) + (subblock >> w_log));

  const int plane = block.y & 3;
  const int2 dims = cb_dims[plane > 0].xy;

  const int noskip = block.y & NoSkipFlag;

  int mv = block.z;
  int mvx = x + ((mv) >> (16 + SUBPEL_BITS)) - 3;
  int mvy = y + ((mv << 16) >> (16 + SUBPEL_BITS)) - 3;
  mvx = clamp(mvx, -11, dims.x);

  int filter_h = (((block.y >> 5) & 15) << 4) + ((mv >> 16) & SUBPEL_MASK);
  int filter_v = (((block.y >> 9) & 15) << 4) + (mv & SUBPEL_MASK);

  int refplane = ((block.y >> 2) & 7) * 3 + plane;
  int ref_offset = cb_refplanes[refplane].y;
  int ref_stride = cb_refplanes[refplane].x;

  int4 kernel_h0 = cb_kernels[filter_h][0];
  int4 kernel_h1 = cb_kernels[filter_h][1];
  int4 kernel_v0 = cb_kernels[filter_v][0];
  int4 kernel_v1 = cb_kernels[filter_v][1];

  int4 output[2] = {{SUM1, SUM1, SUM1, SUM1}, {SUM1, SUM1, SUM1, SUM1}};

  int4 l;
  l = filter_line(dst_frame, ref_offset + mvx + ref_stride * clamp(mvy + 0, 0, dims.y), kernel_h0, kernel_h1);
  output[0] += l * kernel_v0.x;
  l = filter_line(dst_frame, ref_offset + mvx + ref_stride * clamp(mvy + 1, 0, dims.y), kernel_h0, kernel_h1);
  output[1] += l * kernel_v0.x;
  output[0] += l * kernel_v0.y;
  l = filter_line(dst_frame, ref_offset + mvx + ref_stride * clamp(mvy + 2, 0, dims.y), kernel_h0, kernel_h1);
  // output[2] += l * kernel_v0.x;
  output[1] += l * kernel_v0.y;
  output[0] += l * kernel_v0.z;
  l = filter_line(dst_frame, ref_offset + mvx + ref_stride * clamp(mvy + 3, 0, dims.y), kernel_h0, kernel_h1);
  // output[3] += l * kernel_v0.x;
  // output[2] += l * kernel_v0.y;
  output[1] += l * kernel_v0.z;
  output[0] += l * kernel_v0.w;
  l = filter_line(dst_frame, ref_offset + mvx + ref_stride * clamp(mvy + 4, 0, dims.y), kernel_h0, kernel_h1);
  // output[3] += l * kernel_v0.y;
  // output[2] += l * kernel_v0.z;
  output[1] += l * kernel_v0.w;
  output[0] += l * kernel_v1.x;
  l = filter_line(dst_frame, ref_offset + mvx + ref_stride * clamp(mvy + 5, 0, dims.y), kernel_h0, kernel_h1);
  // output[3] += l * kernel_v0.z;
  // output[2] += l * kernel_v0.w;
  output[1] += l * kernel_v1.x;
  output[0] += l * kernel_v1.y;
  l = filter_line(dst_frame, ref_offset + mvx + ref_stride * clamp(mvy + 6, 0, dims.y), kernel_h0, kernel_h1);
  // output[3] += l * kernel_v0.w;
  // output[2] += l * kernel_v1.x;
  output[1] += l * kernel_v1.y;
  output[0] += l * kernel_v1.z;
  l = filter_line(dst_frame, ref_offset + mvx + ref_stride * clamp(mvy + 7, 0, dims.y), kernel_h0, kernel_h1);
  // output[3] += l * kernel_v1.x;
  // output[2] += l * kernel_v1.y;
  output[1] += l * kernel_v1.z;
  output[0] += l * kernel_v1.w;
  l = filter_line(dst_frame, ref_offset + mvx + ref_stride * clamp(mvy + 8, 0, dims.y), kernel_h0, kernel_h1);
  // output[3] += l * kernel_v1.y;
  // output[2] += l * kernel_v1.z;
  output[1] += l * kernel_v1.w;
  // l = filter_line(dst_frame, ref_offset + mvx + ref_stride * clamp(mvy + 9, 0, dims.y), kernel_h0, kernel_h1);
  // output[3] += l * kernel_v1.z;
  // output[2] += l * kernel_v1.w;
  // l = filter_line(dst_frame, ref_offset + mvx + ref_stride * clamp(mvy + 10, 0, dims.y), kernel_h0, kernel_h1);
  // output[3] += l * kernel_v1.w;

  mv = block.w;
  mvx = x + ((mv) >> (16 + SUBPEL_BITS)) - 3;
  mvy = y + ((mv << 16) >> (16 + SUBPEL_BITS)) - 3;
  mvx = clamp(mvx, -11, dims.x);

  filter_h = (((block.y >> 5) & 15) << 4) + ((mv >> 16) & SUBPEL_MASK);
  filter_v = (((block.y >> 9) & 15) << 4) + (mv & SUBPEL_MASK);

  refplane = ((block.y >> 14) & 7) * 3 + plane;
  ref_offset = cb_refplanes[refplane].y;
  ref_stride = cb_refplanes[refplane].x;

  kernel_h0 = cb_kernels[filter_h][0];
  kernel_h1 = cb_kernels[filter_h][1];
  kernel_v0 = cb_kernels[filter_v][0];
  kernel_v1 = cb_kernels[filter_v][1];

  int4 output1[2] = {{SUM1, SUM1, SUM1, SUM1}, {SUM1, SUM1, SUM1, SUM1}};

  l = filter_line(dst_frame, ref_offset + mvx + ref_stride * clamp(mvy + 0, 0, dims.y), kernel_h0, kernel_h1);
  output1[0] += l * kernel_v0.x;
  l = filter_line(dst_frame, ref_offset + mvx + ref_stride * clamp(mvy + 1, 0, dims.y), kernel_h0, kernel_h1);
  output1[1] += l * kernel_v0.x;
  output1[0] += l * kernel_v0.y;
  l = filter_line(dst_frame, ref_offset + mvx + ref_stride * clamp(mvy + 2, 0, dims.y), kernel_h0, kernel_h1);
  // output1[2] += l * kernel_v0.x;
  output1[1] += l * kernel_v0.y;
  output1[0] += l * kernel_v0.z;
  l = filter_line(dst_frame, ref_offset + mvx + ref_stride * clamp(mvy + 3, 0, dims.y), kernel_h0, kernel_h1);
  // output1[3] += l * kernel_v0.x;
  // output1[2] += l * kernel_v0.y;
  output1[1] += l * kernel_v0.z;
  output1[0] += l * kernel_v0.w;
  l = filter_line(dst_frame, ref_offset + mvx + ref_stride * clamp(mvy + 4, 0, dims.y), kernel_h0, kernel_h1);
  // output1[3] += l * kernel_v0.y;
  // output1[2] += l * kernel_v0.z;
  output1[1] += l * kernel_v0.w;
  output1[0] += l * kernel_v1.x;
  l = filter_line(dst_frame, ref_offset + mvx + ref_stride * clamp(mvy + 5, 0, dims.y), kernel_h0, kernel_h1);
  // output1[3] += l * kernel_v0.z;
  // output1[2] += l * kernel_v0.w;
  output1[1] += l * kernel_v1.x;
  output1[0] += l * kernel_v1.y;
  l = filter_line(dst_frame, ref_offset + mvx + ref_stride * clamp(mvy + 6, 0, dims.y), kernel_h0, kernel_h1);
  // output1[3] += l * kernel_v0.w;
  // output1[2] += l * kernel_v1.x;
  output1[1] += l * kernel_v1.y;
  output1[0] += l * kernel_v1.z;
  l = filter_line(dst_frame, ref_offset + mvx + ref_stride * clamp(mvy + 7, 0, dims.y), kernel_h0, kernel_h1);
  // output1[3] += l * kernel_v1.x;
  // output1[2] += l * kernel_v1.y;
  output1[1] += l * kernel_v1.z;
  output1[0] += l * kernel_v1.w;
  l = filter_line(dst_frame, ref_offset + mvx + ref_stride * clamp(mvy + 8, 0, dims.y), kernel_h0, kernel_h1);
  // output1[3] += l * kernel_v1.y;
  // output1[2] += l * kernel_v1.z;
  output1[1] += l * kernel_v1.w;
  // l = filter_line(dst_frame, ref_offset + mvx + ref_stride * clamp(mvy + 9, 0, dims.y), kernel_h0, kernel_h1);
  // output1[3] += l * kernel_v1.z;
  // output1[2] += l * kernel_v1.w;
  // l = filter_line(dst_frame, ref_offset + mvx + ref_stride * clamp(mvy + 10, 0, dims.y), kernel_h0, kernel_h1);
  // output1[3] += l * kernel_v1.w;

  const int output_stride = cb_planes[plane].x;
  const int output_offset = cb_planes[plane].y + x + y * output_stride;

  int coef0 = (block.y >> 17) & 15;
  int coef1 = (block.y >> 21) & 15;
  const int res_stride = cb_planes[plane].z;
  const int res_offset = cb_planes[plane].w;

  for (int i = 0; i < 2; ++i) {
    int4 pix4;
    pix4.x = blend(output[i].x, output1[i].x, coef0, coef1);
    pix4.y = blend(output[i].y, output1[i].y, coef0, coef1);
    pix4.z = blend(output[i].z, output1[i].z, coef0, coef1);
    pix4.w = blend(output[i].w, output1[i].w, coef0, coef1);

    if (noskip) {
      int2 r = (int2)residuals.Load2(res_offset + (x << 1) + (y + i) * res_stride);
      pix4.x += (r.x << 16) >> 16;
      pix4.y += r.x >> 16;
      pix4.z += (r.y << 16) >> 16;
      pix4.w += r.y >> 16;
      pix4 = clamp(pix4, 0, 255);
    }
    dst_frame.Store(output_offset + i * output_stride, pix4.x | (pix4.y << 8) | (pix4.z << 16) | (pix4.w << 24));
  }
}
