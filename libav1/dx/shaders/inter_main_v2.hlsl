/*
 * Copyright 2020 Google LLC
 *
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

// Same size lists and 4x4 output per lane as inter_main.
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
  InterPrediction prediction = inter_predict(x, y, block.z, block.y, refplane, dims);

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
