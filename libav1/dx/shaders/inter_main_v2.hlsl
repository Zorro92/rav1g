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

#define InterRows 4
#include "inter_unscaled.h"

// Same size lists and 4x4 output per lane as inter_main. The phases are common
// to all lanes of a block. Each lane can also take its own path in a mixed wave.
[numthreads(64, 1, 1)] void main(uint3 thread : SV_DispatchThreadID) {
  if (thread.x >= cb_wi_count) return;

  const int w_log = cb_width_log2;
  const int h_log = cb_height_log2;
  const int subblock = thread.x & ((1 << (w_log + h_log)) - 1);
  const int block_index = cb_pass_offset + (thread.x >> (w_log + h_log));
  const uint4 block = pred_blocks.Load4(block_index << 4);
  const int x = 4 * ((block.x & 0xffff) + (subblock & ((1 << w_log) - 1)));
  const int y = 4 * ((block.x >> 16) + (subblock >> w_log));
  const int plane = block.y & 3;
  const int refplane = ((block.y >> 2) & 7) * 3 + plane;
  const int2 dims = cb_dims[plane > 0].xy;
  const int stride = cb_refplanes[refplane].x;

  const int mv = block.z;
  const int ref_x = x + (mv >> (16 + SUBPEL_BITS)) - 3;
  const int ref_y = y + ((int)((uint)mv << 16) >> (16 + SUBPEL_BITS)) - 3;
  // Clamp the window origin before moving to tap 3. Horizontal edge samples
  // are supplied by the reference's extended border.
  const int column = cb_refplanes[refplane].y + clamp(ref_x, -11, dims.x) * InterSampleBytes;
  const int center = column + 3 * InterSampleBytes;
  const int phase_x = (mv >> 16) & SUBPEL_MASK;
  const int phase_y = mv & SUBPEL_MASK;
  const int filter_h = (((block.y >> 5) & 15) << SUBPEL_BITS) + phase_x;
  const int filter_v = (((block.y >> 9) & 15) << SUBPEL_BITS) + phase_y;

  InterPrediction prediction;
  [branch] if (phase_y == 0) {
    [branch] if (phase_x == 0) {
      [unroll] for (int i = 0; i < InterRows; ++i) {
        prediction.row[i] = clamp(inter_load_row(center + stride * clamp(ref_y + 3 + i, 0, dims.y)), 0, InterPixelMax);
      }
    } else {
      // The identity vertical tap leaves four bits after the horizontal round.
      const int4 h0 = cb_kernels[filter_h][0];
      const int4 h1 = cb_kernels[filter_h][1];
      [unroll] for (int i = 0; i < InterRows; ++i) {
        const int4 row = InterFilterLine(dst_frame, column + stride * clamp(ref_y + 3 + i, 0, dims.y), h0, h1);
        prediction.row[i] = clamp(((8 - InterLineBias) + row) >> 4, 0, InterPixelMax);
      }
    }
  } else {
    const int4 v0 = cb_kernels[filter_v][0];
    const int4 v1 = cb_kernels[filter_v][1];
    [branch] if (phase_x == 0) {
      const int4 unused = int4(0, 0, 0, 0);
      prediction = inter_vertical(center, stride, ref_y, dims.y, unused, unused, v0, v1, false);
      // The identity horizontal tap scales samples by 16 and the vertical
      // taps sum to 128, so the horizontal bias cancels at the final round.
      [unroll] for (int i = 0; i < InterRows; ++i) {
        prediction.row[i] = clamp((64 + prediction.row[i]) >> 7, 0, InterPixelMax);
      }
    } else {
      prediction = inter_vertical(column, stride, ref_y, dims.y, cb_kernels[filter_h][0], cb_kernels[filter_h][1],
                                  v0, v1, true);
      // The horizontal pass already includes the line bias. The other
      // legacy offset is an exact multiple of the final rounding scale.
      [unroll] for (int i = 0; i < InterRows; ++i) {
        prediction.row[i] = clamp((1024 - 128 * InterLineBias + prediction.row[i]) >> 11, 0, InterPixelMax);
      }
    }
  }

  const int4 dst = cb_planes[plane];
  [branch] if (block.y & NoSkipFlag) {
    [unroll] for (int i = 0; i < InterRows; ++i) {
      prediction.row[i] = inter_add_residual(dst.w + x * 2 + (y + i) * dst.z, prediction.row[i]);
    }
  }
  [unroll] for (int i = 0; i < InterRows; ++i) {
    inter_store_row(dst.y + x * InterSampleBytes + (y + i) * dst.x, prediction.row[i]);
  }
}
