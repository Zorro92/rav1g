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
#define OffsetBits 19
#define OutputRoundAdd ((1 << (OutputShift - 1)) + (1 << OffsetBits))
#define OutputSub ((1 << (OffsetBits - OutputShift)) + (1 << (OffsetBits - OutputShift - 1)))
#define LocalStride 20

groupshared int intermediate_buffer[64 * LocalStride];

[numthreads(64, 1, 1)] void main(uint3 thread
                                 : SV_DispatchThreadID) {
  // A thread behind the last block of the dispatch does nothing, but it takes part in the group syncs: a
  // group sync has to be reached by all threads of the group.
  const bool active = thread.x < cb_wi_count;

  const int w_log = cb_width_log2;
  const int h_log = cb_height_log2;
  const int subblock = (thread.x >> 2) & ((1 << (w_log + h_log)) - 1);
  const int block_index = cb_pass_offset + (thread.x >> (w_log + h_log + 2));
  uint4 block = uint4(0, 0, 0, 0);
  [branch] if (active) block = pred_blocks.Load4(block_index << 4);

  const int wi = thread.x & 3;
  const int dx = SubblockW * (subblock & ((1 << w_log) - 1));
  const int dy = SubblockH * (subblock >> w_log);
  const int plane = block.y & 3;
  const int noskip = block.y & NoSkipFlag;
  const int ref_frm = (block.y >> 2) & 7;
  const int refplane = ref_frm * 3 + plane;
  const int ref_offset = cb_refplanes[refplane].y;
  const int ref_stride = cb_refplanes[refplane].x;
  const int ref_w = cb_refplanes[refplane].z;
  const int ref_h = cb_refplanes[refplane].w;

  int4 scale = cb_scale[ref_frm + 1];
  int mbx = SubblockW * (block.x & 0xffff);
  int mby = SubblockH * (block.x >> 16);
  int mv = block.z;
  int mvx = scale_value((mbx << SUBPEL_BITS) + (mv >> 16), scale.x) + SCALE_EXTRA_OFF;
  int mvy = scale_value((mby << SUBPEL_BITS) + ((mv << 16) >> 16), scale.z) + SCALE_EXTRA_OFF;
  mvx += (dx + wi) * scale.y;
  mvy += dy * scale.w;
  int x0 = clamp((mvx >> SCALE_SUBPEL_BITS) - 3, -11, ref_w);
  int y0 = (mvy >> SCALE_SUBPEL_BITS) - 3;
  mvx &= SCALE_SUBPEL_MASK;
  mvy &= SCALE_SUBPEL_MASK;

  mbx += dx;
  mby += dy + wi;

  const int filter_h = (((block.y >> 5) & 15) << 4) + (mvx >> SCALE_EXTRA_BITS);
  const int lines = 8 + ((3 * scale.w + mvy) >> SCALE_SUBPEL_BITS);

  int4 kernel_h0 = cb_kernels[filter_h][0];
  int4 kernel_h1 = cb_kernels[filter_h][1];
  int local_base = (thread.x & 63) * LocalStride;
  int i;
  if (active) for (i = 0; i < lines; ++i) {
    int ref_addr = ref_offset + ref_stride * clamp(y0 + i, 0, ref_h) + x0;
    uint3 l = dst_frame.Load3(ref_addr & (~3));
    const uint shift = (ref_addr & 3) * 8;
    l.x = (l.x >> shift) | ((l.y << (24 - shift)) << 8);
    l.y = (l.y >> shift) | ((l.z << (24 - shift)) << 8);
    int sum = 0;
    sum += kernel_h0.x * (int)((l.x >> 0) & 0xff);
    sum += kernel_h0.y * (int)((l.x >> 8) & 0xff);
    sum += kernel_h0.z * (int)((l.x >> 16) & 0xff);
    sum += kernel_h0.w * (int)((l.x >> 24) & 0xff);
    sum += kernel_h1.x * (int)((l.y >> 0) & 0xff);
    sum += kernel_h1.y * (int)((l.y >> 8) & 0xff);
    sum += kernel_h1.z * (int)((l.y >> 16) & 0xff);
    sum += kernel_h1.w * (int)((l.y >> 24) & 0xff);
    intermediate_buffer[local_base + i] = (sum + FilterLineAdd8bit) >> FilterLineShift;
  }

  // The lines filtered horizontally are handed from the threads that made them to the threads that filter
  // them vertically, through the group memory. The threads of a group run in no particular order, so it
  // takes a group sync to have all lines there before any of them is read. (Upstream had plain group
  // memory barriers, which make no thread wait: that held only where the 64 threads of a group ran in
  // lockstep, as one wave.)
  GroupMemoryBarrierWithGroupSync();
  if (!active) return;

  mvy += wi * scale.w;
  const int filter_v = (((block.y >> 9) & 15) << 4) + ((mvy & SCALE_SUBPEL_MASK) >> SCALE_EXTRA_BITS);
  const int4 kernel_v0 = cb_kernels[filter_v][0];
  const int4 kernel_v1 = cb_kernels[filter_v][1];
  local_base = (mvy >> SCALE_SUBPEL_BITS) + (thread.x & 60) * LocalStride;
  int output[4];  /// int4???
  for (i = 0; i < 4; ++i) {
    int sum = 0;
    int loc_addr = local_base + i * LocalStride;
    sum += kernel_v0.x * intermediate_buffer[loc_addr + 0];
    sum += kernel_v0.y * intermediate_buffer[loc_addr + 1];
    sum += kernel_v0.z * intermediate_buffer[loc_addr + 2];
    sum += kernel_v0.w * intermediate_buffer[loc_addr + 3];
    sum += kernel_v1.x * intermediate_buffer[loc_addr + 4];
    sum += kernel_v1.y * intermediate_buffer[loc_addr + 5];
    sum += kernel_v1.z * intermediate_buffer[loc_addr + 6];
    sum += kernel_v1.w * intermediate_buffer[loc_addr + 7];
    output[i] = clamp((int)(((sum + OutputRoundAdd) >> OutputShift) - OutputSub), 0, 255);
  }

  if (noskip) {
    const int res_addr = cb_planes[plane].w + (mbx << 1) + mby * cb_planes[plane].z;
    int2 r = (int2)residuals.Load2(res_addr);
    output[0] = clamp(output[0] + ((r.x << 16) >> 16), 0, 255);
    output[1] = clamp(output[1] + (r.x >> 16), 0, 255);
    output[2] = clamp(output[2] + ((r.y << 16) >> 16), 0, 255);
    output[3] = clamp(output[3] + (r.y >> 16), 0, 255);
  }

  const int output_addr = cb_planes[plane].y + mbx + mby * cb_planes[plane].x;
  dst_frame.Store(output_addr, output[0] | (output[1] << 8) | (output[2] << 16) | (output[3] << 24));
}
