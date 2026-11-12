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

// Test flag -interfast: inter_main.hlsl for the blocks whose motion vector has no fraction in x, in y, or
// in both (classes 0 to 2 of inter_fast_common.h; the blocks of class 3 stay with inter_main.hlsl). The block
// records, the thread layout (a thread predicts 4x4 samples), the clamps and the residual are those of
// inter_main.hlsl; the output is the same. inter_fast_hbd.hlsl builds this file for 16-bit samples.
//
// What a thread loads and computes, against the 11 rows of 11 samples and the two passes of inter_main:
//   copy  4 rows of 4 samples, stored as they are
//   h     4 rows of 11 samples, one horizontal pass:  (sum + 4) >> 3, then (+ 8) >> 4
//   v     11 rows of 4 samples, one vertical pass:    (sum + 64) >> 7

#include "inter_common.h"
#include "inter_fast_common.h"

#define SubblockW 4
#define SubblockH 4

[numthreads(64, 1, 1)] void main(uint3 thread
                                 : SV_DispatchThreadID) {
  if (thread.x >= cb_wi_count) return;

  const int w_log = cb_width_log2 & 255;
  const int h_log = cb_height_log2;
  const int frac_class = cb_width_log2 >> 8;
  const int subblock = thread.x & ((1 << (w_log + h_log)) - 1);
  const int block_index = cb_pass_offset + (thread.x >> (w_log + h_log));
  uint4 block = pred_blocks.Load4(block_index << 4);
  // The record of inter_main.hlsl: block.x position, block.y plane, reference, filters, skip, block.z vector.

  int x = SubblockW * ((block.x & 0xffff) + (subblock & ((1 << w_log) - 1)));
  int y = SubblockH * ((block.x >> 16) + (subblock >> w_log));

  const int plane = block.y & 3;
  const int2 dims = cb_dims[plane > 0].xy;

  const int noskip = block.y & NoSkipFlag;

  int mv = block.z;
  int mvx = x + ((mv) >> (16 + SUBPEL_BITS)) - 3;
  int mvy = y + ((mv << 16) >> (16 + SUBPEL_BITS)) - 3;
  // The first of the 11 samples inter_main reads of a row; the sample under tap 3 is the block's own column.
  mvx = clamp(mvx, -11, dims.x) << InterFastSampleShift;

  const int filter_h = (((block.y >> 5) & 15) << 4) + ((mv >> 16) & SUBPEL_MASK);
  const int filter_v = (((block.y >> 9) & 15) << 4) + (mv & SUBPEL_MASK);

  const int refplane = ((block.y >> 2) & 7) * 3 + plane;
  const int ref_offset = cb_refplanes[refplane].y;
  const int ref_stride = cb_refplanes[refplane].x;

  int4 output[4];
  int i;
  [branch] if (frac_class == InterFastCopy) {
    const int column = ref_offset + mvx + (3 << InterFastSampleShift);
    // Limited like every result of inter_main: nothing for the samples of a decoded frame.
    for (i = 0; i < 4; ++i)
      output[i] = inter_fast_round(inter_fast_row(dst_frame, column + ref_stride * clamp(mvy + 3 + i, 0, dims.y)), 0, 0);
  } else if (frac_class == InterFastH) {
    const int4 kernel_h0 = cb_kernels[filter_h][0];
    const int4 kernel_h1 = cb_kernels[filter_h][1];
    for (i = 0; i < 4; ++i) {
      const int4 l = InterFastLine(dst_frame, ref_offset + mvx + ref_stride * clamp(mvy + 3 + i, 0, dims.y),
                                   kernel_h0, kernel_h1);
      output[i] = inter_fast_round(l, 8 - InterFastLineBias, 4);
    }
  } else {
    const int4 kernel_v0 = cb_kernels[filter_v][0];
    const int4 kernel_v1 = cb_kernels[filter_v][1];
    const int column = ref_offset + mvx + (3 << InterFastSampleShift);
    int4 rows[11];
    for (i = 0; i < 11; ++i) rows[i] = inter_fast_row(dst_frame, column + ref_stride * clamp(mvy + i, 0, dims.y));
    for (i = 0; i < 4; ++i) {
      const int4 sum = inter_fast_taps(rows[i], rows[i + 1], rows[i + 2], rows[i + 3], rows[i + 4], rows[i + 5],
                                       rows[i + 6], rows[i + 7], kernel_v0, kernel_v1);
      output[i] = inter_fast_round(sum, 64, 7);
    }
  }

  x <<= InterFastSampleShift;
  const int output_stride = cb_planes[plane].x;
  const int output_offset = cb_planes[plane].y + x + y * output_stride;
  const int res_stride = cb_planes[plane].z;
  const int res_offset = cb_planes[plane].w + (x << (1 - InterFastSampleShift)) + y * res_stride;
  for (i = 0; i < 4; ++i) {
    if (noskip) {
      int2 r = (int2)residuals.Load2(res_offset + i * res_stride);
      output[i].x += (r.x << 16) >> 16;
      output[i].y += r.x >> 16;
      output[i].z += (r.y << 16) >> 16;
      output[i].w += r.y >> 16;
      output[i] = clamp(output[i], 0, InterFastPixelMax);
    }
#ifdef INTER_FAST_HBD
    dst_frame.Store2(output_offset + i * output_stride,
                     uint2(output[i].x | (output[i].y << 16), output[i].z | (output[i].w << 16)));
#else
    dst_frame.Store(output_offset + i * output_stride,
                    output[i].x | (output[i].y << 8) | (output[i].z << 16) | (output[i].w << 24));
#endif
  }
}
