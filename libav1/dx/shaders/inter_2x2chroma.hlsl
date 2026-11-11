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
#define OutputRoundAdd (1 << (OutputShift - 1))
#define OffsetBits 19
#define SumAdd (1 << OffsetBits)
#define OutputSub ((1 << (OffsetBits - OutputShift)) + (1 << (OffsetBits - OutputShift - 1)))
#define DualWriteBlock (1 << 25)
#define SUM1 1 << OffsetBits

groupshared uint2 mem[64];

// The prediction of the block of one thread: its record, its two lines as the halves of two words of the
// frame, and where they go. False for a thread behind the last block of the dispatch, which has no block.
bool predict(uint3 thread, out uint4 block, out int x, out uint2 output, out int output_offset, out int output_stride) {
  block = uint4(0, 0, 0, 0);
  x = 0;
  output = uint2(0, 0);
  output_offset = 0;
  output_stride = 0;
  if (thread.x >= cb_wi_count) return false;

  block = pred_blocks.Load4((cb_pass_offset + thread.x) * 16);

  x = SubblockW * (block.x & 0xffff);
  int y = SubblockH * (block.x >> 16);

  const int2 dims = cb_dims[1].xy;
  const int plane = block.y & 3;
  int noskip = block.y & NoSkipFlag;

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

  int4 tmp = {SUM1, SUM1, SUM1, SUM1};

  int2 l;
  l = filter_line2(dst_frame, ref_offset + mvx + ref_stride * clamp(mvy + 0, 0, dims.y), kernel_h0, kernel_h1);
  tmp.xy += l * cb_kernels[filter_v][0].x;
  l = filter_line2(dst_frame, ref_offset + mvx + ref_stride * clamp(mvy + 1, 0, dims.y), kernel_h0, kernel_h1);
  tmp.zw += l * cb_kernels[filter_v][0].x;
  tmp.xy += l * cb_kernels[filter_v][0].y;
  l = filter_line2(dst_frame, ref_offset + mvx + ref_stride * clamp(mvy + 2, 0, dims.y), kernel_h0, kernel_h1);
  tmp.zw += l * cb_kernels[filter_v][0].y;
  tmp.xy += l * cb_kernels[filter_v][0].z;
  l = filter_line2(dst_frame, ref_offset + mvx + ref_stride * clamp(mvy + 3, 0, dims.y), kernel_h0, kernel_h1);
  tmp.zw += l * cb_kernels[filter_v][0].z;
  tmp.xy += l * cb_kernels[filter_v][0].w;
  l = filter_line2(dst_frame, ref_offset + mvx + ref_stride * clamp(mvy + 4, 0, dims.y), kernel_h0, kernel_h1);
  tmp.zw += l * cb_kernels[filter_v][0].w;
  tmp.xy += l * cb_kernels[filter_v][1].x;
  l = filter_line2(dst_frame, ref_offset + mvx + ref_stride * clamp(mvy + 5, 0, dims.y), kernel_h0, kernel_h1);
  tmp.zw += l * cb_kernels[filter_v][1].x;
  tmp.xy += l * cb_kernels[filter_v][1].y;
  l = filter_line2(dst_frame, ref_offset + mvx + ref_stride * clamp(mvy + 6, 0, dims.y), kernel_h0, kernel_h1);
  tmp.zw += l * cb_kernels[filter_v][1].y;
  tmp.xy += l * cb_kernels[filter_v][1].z;
  l = filter_line2(dst_frame, ref_offset + mvx + ref_stride * clamp(mvy + 7, 0, dims.y), kernel_h0, kernel_h1);
  tmp.zw += l * cb_kernels[filter_v][1].z;
  tmp.xy += l * cb_kernels[filter_v][1].w;
  l = filter_line2(dst_frame, ref_offset + mvx + ref_stride * clamp(mvy + 8, 0, dims.y), kernel_h0, kernel_h1);
  tmp.zw += l * cb_kernels[filter_v][1].w;
  tmp.x = clamp((int)(((tmp.x + OutputRoundAdd) >> OutputShift) - OutputSub), 0, 255);
  tmp.y = clamp((int)(((tmp.y + OutputRoundAdd) >> OutputShift) - OutputSub), 0, 255);
  tmp.z = clamp((int)(((tmp.z + OutputRoundAdd) >> OutputShift) - OutputSub), 0, 255);
  tmp.w = clamp((int)(((tmp.w + OutputRoundAdd) >> OutputShift) - OutputSub), 0, 255);

  /// add residuals here

  const int res_stride = cb_planes[plane].z;
  const int res_offset = cb_planes[plane].w + (x << 1) + y * res_stride;
  if (noskip) {
    int r0 = residuals.Load(res_offset);
    int r1 = residuals.Load(res_offset + res_stride);
    tmp.x += (r0 << 16) >> 16;
    tmp.y += r0 >> 16;
    tmp.z += (r1 << 16) >> 16;
    tmp.w += r1 >> 16;
    tmp = clamp(tmp, 0, 255);
  }

  output.x = (tmp.x | (tmp.y << 8)) << ((x & 2) * 8);
  output.y = (tmp.z | (tmp.w << 8)) << ((x & 2) * 8);

  output_stride = cb_planes[plane].x;
  output_offset = cb_planes[plane].y + (x & (~3)) + y * output_stride;
  return true;
}

[numthreads(64, 1, 1)] void main(uint3 thread
                                 : SV_DispatchThreadID) {
  uint4 block;
  int x;
  uint2 output;
  int output_offset;
  int output_stride;
  const bool active = predict(thread, block, x, output, output_offset, output_stride);

  // Two 2x2 blocks that share a word of the frame and are predicted in this dispatch are written as one
  // word: both put their half into the group memory, and the first of the pair stores the two. The threads
  // of a group run in no particular order, so it takes a group sync to have the halves there before they
  // are read, and a group sync has to be reached by all threads of the group, those without a block too.
  // (Upstream had a plain group memory barrier inside the branch, which makes no thread wait: that held
  // only where the 64 threads of a group ran in lockstep, as one wave.)
  if (active && (block.y & DualWriteBlock)) mem[thread.x & 63] = output;
  GroupMemoryBarrierWithGroupSync();
  if (!active) return;

  if (block.y & DualWriteBlock) {
    if ((thread.x & 1) == 0) {
      int2 output1 = mem[(thread.x & 63) + 1];
      output.x |= output1.x;
      output.y |= output1.y;
      dst_frame.Store(output_offset, output.x);
      dst_frame.Store(output_offset + output_stride, output.y);
    }
  } else {
    const uint mask = 0xffff0000 >> ((x & 2) * 8);
    output.x |= dst_frame.Load(output_offset) & mask;
    output.y |= dst_frame.Load(output_offset + output_stride) & mask;
    dst_frame.Store(output_offset, output.x);
    dst_frame.Store(output_offset + output_stride, output.y);
  }
}
