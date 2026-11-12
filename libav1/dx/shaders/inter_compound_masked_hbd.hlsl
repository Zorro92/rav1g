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
#define OffsetBits 21
#define OutputRoundAdd ((1 << (OutputShift - 1)) + (1 << OffsetBits))
#define OutputSub ((1 << (OffsetBits - OutputShift)) + (1 << (OffsetBits - OutputShift - 1)))
#define RoundFinal 4
#define DistBits 4
#define PixelMax 1023
#define MaskBits 6
#define MaskMax 64

int blend(int src0, int src1, int mask) {
  src0 = (src0 + OutputRoundAdd) >> OutputShift;
  src1 = (src1 + OutputRoundAdd) >> OutputShift;
  int result = (src0 * mask + src1 * (MaskMax - mask)) >> MaskBits;  // maybe this needs rounding (+32 before shift)
  result = (result - OutputSub + (1 << (RoundFinal - 1))) >> RoundFinal;
  return clamp(result, 0, PixelMax);
}

[numthreads(64, 1, 1)] void main(uint3 thread
                                 : SV_DispatchThreadID) {
  if (thread.x >= cb_wi_count) return;

  const int w_log = cb_width_log2;
  const int h_log = cb_height_log2;
  const int subblock = thread.x & ((1 << (w_log + h_log)) - 1);

  uint4 block = pred_blocks.Load4((cb_pass_offset + (thread.x >> (w_log + h_log))) * 16);
  // block.x - pos xy
  // block.y - flags:
  //        2    plane
  //        3    ref
  //        4    filter_x
  //        4    filter_y
  //        1    skip
  //        3    ref1
  //
  int x = SubblockW * ((block.x & 0xffff) + low_bits(subblock, w_log));
  int y = SubblockH * (((block.x >> 16) << 1) + (subblock >> w_log));

  const int plane = block.y & 3;
  const int2 dims = cb_dims[plane > 0].xy;

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

  int4 output[2] = {{0, 0, 0, 0}, {0, 0, 0, 0}};

  int4 l;
  l = filter_line_hbd(dst_frame, ref_offset + mvx + ref_stride * clamp(mvy + 0, 0, dims.y), kernel_h0, kernel_h1);
  output[0] += l * kernel_v0.x;
  l = filter_line_hbd(dst_frame, ref_offset + mvx + ref_stride * clamp(mvy + 1, 0, dims.y), kernel_h0, kernel_h1);
  output[1] += l * kernel_v0.x;
  output[0] += l * kernel_v0.y;
  l = filter_line_hbd(dst_frame, ref_offset + mvx + ref_stride * clamp(mvy + 2, 0, dims.y), kernel_h0, kernel_h1);
  output[1] += l * kernel_v0.y;
  output[0] += l * kernel_v0.z;
  l = filter_line_hbd(dst_frame, ref_offset + mvx + ref_stride * clamp(mvy + 3, 0, dims.y), kernel_h0, kernel_h1);
  output[1] += l * kernel_v0.z;
  output[0] += l * kernel_v0.w;
  l = filter_line_hbd(dst_frame, ref_offset + mvx + ref_stride * clamp(mvy + 4, 0, dims.y), kernel_h0, kernel_h1);
  output[1] += l * kernel_v0.w;
  output[0] += l * kernel_v1.x;
  l = filter_line_hbd(dst_frame, ref_offset + mvx + ref_stride * clamp(mvy + 5, 0, dims.y), kernel_h0, kernel_h1);
  output[1] += l * kernel_v1.x;
  output[0] += l * kernel_v1.y;
  l = filter_line_hbd(dst_frame, ref_offset + mvx + ref_stride * clamp(mvy + 6, 0, dims.y), kernel_h0, kernel_h1);
  output[1] += l * kernel_v1.y;
  output[0] += l * kernel_v1.z;
  l = filter_line_hbd(dst_frame, ref_offset + mvx + ref_stride * clamp(mvy + 7, 0, dims.y), kernel_h0, kernel_h1);
  output[1] += l * kernel_v1.z;
  output[0] += l * kernel_v1.w;
  l = filter_line_hbd(dst_frame, ref_offset + mvx + ref_stride * clamp(mvy + 8, 0, dims.y), kernel_h0, kernel_h1);
  output[1] += l * kernel_v1.w;

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

  // The addresses of the nine rows of the second reference, as two vectors and a scalar. Written row by
  // row in the calls below, FXC made rows 2..5 one vector over the register that held mvy and two values
  // read together just before it (iadd r4.xyzw, r4.zzzz, l(2, 3, 4, 5) after imad r3.x, ..., r4.w, r4.x):
  // the pair that testing/dxbc_scan/scan.py refuses (docs/gpu-av1-decoder.md, "A paired read before an
  // overlapping in-place write"). The values are the same.
  const int4 rows_a = ref_offset + mvx + ref_stride * clamp(mvy + int4(0, 1, 2, 3), 0, dims.y);
  const int4 rows_b = ref_offset + mvx + ref_stride * clamp(mvy + int4(4, 5, 6, 7), 0, dims.y);
  const int row_8 = ref_offset + mvx + ref_stride * clamp(mvy + 8, 0, dims.y);

  int4 output1[2] = {{0, 0, 0, 0}, {0, 0, 0, 0}};

  l = filter_line_hbd(dst_frame, rows_a.x, kernel_h0, kernel_h1);
  output1[0] += l * kernel_v0.x;
  l = filter_line_hbd(dst_frame, rows_a.y, kernel_h0, kernel_h1);
  output1[1] += l * kernel_v0.x;
  output1[0] += l * kernel_v0.y;
  l = filter_line_hbd(dst_frame, rows_a.z, kernel_h0, kernel_h1);
  output1[1] += l * kernel_v0.y;
  output1[0] += l * kernel_v0.z;
  l = filter_line_hbd(dst_frame, rows_a.w, kernel_h0, kernel_h1);
  output1[1] += l * kernel_v0.z;
  output1[0] += l * kernel_v0.w;
  l = filter_line_hbd(dst_frame, rows_b.x, kernel_h0, kernel_h1);
  output1[1] += l * kernel_v0.w;
  output1[0] += l * kernel_v1.x;
  l = filter_line_hbd(dst_frame, rows_b.y, kernel_h0, kernel_h1);
  output1[1] += l * kernel_v1.x;
  output1[0] += l * kernel_v1.y;
  l = filter_line_hbd(dst_frame, rows_b.z, kernel_h0, kernel_h1);
  output1[1] += l * kernel_v1.y;
  output1[0] += l * kernel_v1.z;
  l = filter_line_hbd(dst_frame, rows_b.w, kernel_h0, kernel_h1);
  output1[1] += l * kernel_v1.z;
  output1[0] += l * kernel_v1.w;
  l = filter_line_hbd(dst_frame, row_8, kernel_h0, kernel_h1);
  output1[1] += l * kernel_v1.w;

  x <<= 1;
  const int output_stride = cb_planes[plane].x;
  const int output_offset = cb_planes[plane].y + x + y * output_stride;

  int wedge_stride = SubblockW << w_log;
  int wedge_addr = (((block.y >> 17) & 0x1fff) << 6) + SubblockW * low_bits(subblock, w_log) +
                   SubblockH * (subblock >> w_log) * wedge_stride;
  const int res_stride = cb_planes[plane].z;
  const int res_offset = cb_planes[plane].w + x + y * res_stride;
  for (int i = 0; i < 2; ++i) {
    uint wedge = comp_mask.Load(wedge_addr + wedge_stride * i);
    output[i].x = blend(output[i].x, output1[i].x, (wedge >> 0) & 255);
    output[i].y = blend(output[i].y, output1[i].y, (wedge >> 8) & 255);
    output[i].z = blend(output[i].z, output1[i].z, (wedge >> 16) & 255);
    output[i].w = blend(output[i].w, output1[i].w, (wedge >> 24) & 255);
    // A branch, not a select: flattened, the select of the four samples went into the register that held
    // noskip and two values read together just before it (movc r0.xyzw, r0.wwww, r2.xyzw, r4.xyzw after
    // iadd r3.w, r0.z, r0.x), the pair that testing/dxbc_scan/scan.py refuses (docs/gpu-av1-decoder.md,
    // "A paired read before an overlapping in-place write").
    [branch] if (noskip) {
      int2 r = (int2)residuals.Load2(res_offset + i * res_stride);
      output[i].x += (r.x << 16) >> 16;
      output[i].y += r.x >> 16;
      output[i].z += (r.y << 16) >> 16;
      output[i].w += r.y >> 16;
      output[i] = clamp(output[i], 0, PixelMax);
    }
    dst_frame.Store2(output_offset + i * output_stride,
                     uint2(output[i].x | (output[i].y << 16), output[i].z | (output[i].w << 16)));
  }
}
