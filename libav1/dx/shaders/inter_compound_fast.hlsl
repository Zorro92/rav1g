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

// Test flag -interfast: inter_compound.hlsl (two references, averaged or distance weighted) for the blocks
// in which the motion vector of one reference or of both has no fraction in x, in y, or in both. Each
// reference has a class of its own (inter_fast_common.h); the blocks with two references of class 3 stay with
// inter_compound.hlsl. The block records, the thread layout (a thread predicts 4x2 samples), the clamps,
// the weights and the residual are those of inter_compound.hlsl; the output is the same.
// inter_compound_fast_hbd.hlsl builds this file for 16-bit samples.
//
// inter_compound.hlsl keeps four more bits of each reference than a sample has and blends
//   src = (vertical sum + OutputRoundAdd) >> 7,  9 rows of 11 samples per reference.
// That value is here, with p a sample and sum a plain sum of taps times samples:
//   copy  16 * p + OutputSub             2 rows of 4 samples
//   h     ((sum + 4) >> 3) + OutputSub   2 rows of 11 samples; filter_line() has the first part
//   v     ((sum + 4) >> 3) + OutputSub   9 rows of 4 samples
//   2d    as in inter_compound.hlsl      9 rows of 11 samples

#include "inter_common.h"
#include "inter_fast_common.h"

#define SubblockW 4
#define SubblockH 2
#define OutputShift 7
#define RoundFinal 4
#define DistBits 4
#ifdef INTER_FAST_HBD
#define OffsetBits 21
#define OutputRoundAdd ((1 << (OutputShift - 1)) + (1 << OffsetBits))
#else
#define OffsetBits 19
// inter_compound.hlsl starts its sums at 1 << OffsetBits; here it is part of the rounding term.
#define OutputRoundAdd ((1 << (OutputShift - 1)) + (1 << OffsetBits))
#endif
#define OutputSub ((1 << (OffsetBits - OutputShift)) + (1 << (OffsetBits - OutputShift - 1)))

// The two rows of a thread from one reference, as the blend takes them.
struct InterFastRows {
  int4 row[2];
};

int4 comp_fast_scale(int4 v, int mul, int add, int shift, int offset) {
  int4 r;
  r.x = ((v.x * mul + add) >> shift) + offset;
  r.y = ((v.y * mul + add) >> shift) + offset;
  r.z = ((v.z * mul + add) >> shift) + offset;
  r.w = ((v.w * mul + add) >> shift) + offset;
  return r;
}

InterFastRows comp_fast_ref(int frac_class, int x, int y, int mv, int filters, int refplane, int2 dims) {
  int mvx = x + ((mv) >> (16 + SUBPEL_BITS)) - 3;
  const int mvy = y + ((mv << 16) >> (16 + SUBPEL_BITS)) - 3;
  mvx = clamp(mvx, -11, dims.x) << InterFastSampleShift;

  const int filter_h = (((filters >> 5) & 15) << 4) + ((mv >> 16) & SUBPEL_MASK);
  const int filter_v = (((filters >> 9) & 15) << 4) + (mv & SUBPEL_MASK);
  const int ref_offset = cb_refplanes[refplane].y;
  const int ref_stride = cb_refplanes[refplane].x;
  const int row_base = ref_offset + mvx;
  const int column = row_base + (3 << InterFastSampleShift);

  InterFastRows rows;
  int i;
  [branch] if (frac_class == InterFastCopy) {
    rows.row[0] = comp_fast_scale(inter_fast_row(dst_frame, column + ref_stride * clamp(mvy + 3, 0, dims.y)), 16, 0, 0,
                                OutputSub);
    rows.row[1] = comp_fast_scale(inter_fast_row(dst_frame, column + ref_stride * clamp(mvy + 4, 0, dims.y)), 16, 0, 0,
                                OutputSub);
  } else if (frac_class == InterFastH) {
    const int4 kernel_h0 = cb_kernels[filter_h][0];
    const int4 kernel_h1 = cb_kernels[filter_h][1];
    // filter_line() has ((sum + 4) >> 3) + InterFastLineBias.
    rows.row[0] = comp_fast_scale(
        InterFastLine(dst_frame, row_base + ref_stride * clamp(mvy + 3, 0, dims.y), kernel_h0, kernel_h1), 1, 0, 0,
        OutputSub - InterFastLineBias);
    rows.row[1] = comp_fast_scale(
        InterFastLine(dst_frame, row_base + ref_stride * clamp(mvy + 4, 0, dims.y), kernel_h0, kernel_h1), 1, 0, 0,
        OutputSub - InterFastLineBias);
  } else if (frac_class == InterFastV) {
    const int4 kernel_v0 = cb_kernels[filter_v][0];
    const int4 kernel_v1 = cb_kernels[filter_v][1];
    int4 r[9];
    for (i = 0; i < 9; ++i) r[i] = inter_fast_row(dst_frame, column + ref_stride * clamp(mvy + i, 0, dims.y));
    rows.row[0] = comp_fast_scale(inter_fast_taps(r[0], r[1], r[2], r[3], r[4], r[5], r[6], r[7], kernel_v0, kernel_v1), 1,
                                4, 3, OutputSub);
    rows.row[1] = comp_fast_scale(inter_fast_taps(r[1], r[2], r[3], r[4], r[5], r[6], r[7], r[8], kernel_v0, kernel_v1), 1,
                                4, 3, OutputSub);
  } else {
    // Both passes, as inter_compound.hlsl runs them.
    const int4 kernel_h0 = cb_kernels[filter_h][0];
    const int4 kernel_h1 = cb_kernels[filter_h][1];
    const int4 kernel_v0 = cb_kernels[filter_v][0];
    const int4 kernel_v1 = cb_kernels[filter_v][1];
    int4 r[9];
    for (i = 0; i < 9; ++i)
      r[i] = InterFastLine(dst_frame, row_base + ref_stride * clamp(mvy + i, 0, dims.y), kernel_h0, kernel_h1);
    rows.row[0] = comp_fast_scale(inter_fast_taps(r[0], r[1], r[2], r[3], r[4], r[5], r[6], r[7], kernel_v0, kernel_v1), 1,
                                OutputRoundAdd, OutputShift, 0);
    rows.row[1] = comp_fast_scale(inter_fast_taps(r[1], r[2], r[3], r[4], r[5], r[6], r[7], r[8], kernel_v0, kernel_v1), 1,
                                OutputRoundAdd, OutputShift, 0);
  }
  return rows;
}

// blend() of inter_compound.hlsl behind its first two lines.
int blend(int src0, int src1, int coef0, int coef1) {
  int result = (src0 * coef0 + src1 * coef1) >> DistBits;
  result = (result - OutputSub + (1 << (RoundFinal - 1))) >> RoundFinal;
  return clamp(result, 0, InterFastPixelMax);
}

[numthreads(64, 1, 1)] void main(uint3 thread
                                 : SV_DispatchThreadID) {
  if (thread.x >= cb_wi_count) return;

  const int w_log = cb_width_log2 & 255;
  const int h_log = cb_height_log2;
  const int frac_class0 = (cb_width_log2 >> 8) & 3;
  const int frac_class1 = (cb_width_log2 >> 10) & 3;
  const int subblock = thread.x & ((1 << (w_log + h_log)) - 1);
  const int block_index = cb_pass_offset + (thread.x >> (w_log + h_log));
  uint4 block = pred_blocks.Load4(block_index * 16);
  // The record of inter_compound.hlsl: block.x position, block.y plane, references, filters, skip and
  // weights, block.z and block.w the vectors.
  int x = SubblockW * ((block.x & 0xffff) + (subblock & ((1 << w_log) - 1)));
  int y = SubblockH * (((block.x >> 16) << 1) + (subblock >> w_log));

  const int plane = block.y & 3;
  const int2 dims = cb_dims[plane > 0].xy;

  const int noskip = block.y & NoSkipFlag;

  const InterFastRows ref0 =
      comp_fast_ref(frac_class0, x, y, block.z, block.y, ((block.y >> 2) & 7) * 3 + plane, dims);
  const InterFastRows ref1 =
      comp_fast_ref(frac_class1, x, y, block.w, block.y, ((block.y >> 14) & 7) * 3 + plane, dims);

  x <<= InterFastSampleShift;
  const int output_stride = cb_planes[plane].x;
  const int output_offset = cb_planes[plane].y + x + y * output_stride;

  const int coef0 = (block.y >> 17) & 15;
  const int coef1 = (block.y >> 21) & 15;
  const int res_stride = cb_planes[plane].z;
  const int res_offset = cb_planes[plane].w;
  for (int i = 0; i < 2; ++i) {
    const int4 src0 = ref0.row[i];
    const int4 src1 = ref1.row[i];
    int4 pix4;
    pix4.x = blend(src0.x, src1.x, coef0, coef1);
    pix4.y = blend(src0.y, src1.y, coef0, coef1);
    pix4.z = blend(src0.z, src1.z, coef0, coef1);
    pix4.w = blend(src0.w, src1.w, coef0, coef1);

    if (noskip) {
      int2 r = (int2)residuals.Load2(res_offset + (x << (1 - InterFastSampleShift)) + (y + i) * res_stride);
      pix4.x += (r.x << 16) >> 16;
      pix4.y += r.x >> 16;
      pix4.z += (r.y << 16) >> 16;
      pix4.w += r.y >> 16;
      pix4 = clamp(pix4, 0, InterFastPixelMax);
    }
#ifdef INTER_FAST_HBD
    dst_frame.Store2(output_offset + i * output_stride, uint2(pix4.x | (pix4.y << 16), pix4.z | (pix4.w << 16)));
#else
    dst_frame.Store(output_offset + i * output_stride, pix4.x | (pix4.y << 8) | (pix4.z << 16) | (pix4.w << 24));
#endif
  }
}
