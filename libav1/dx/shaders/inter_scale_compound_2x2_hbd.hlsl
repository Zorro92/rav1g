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
#define OutputRoundAdd ((1 << (OutputShift - 1)) + (1 << OffsetBits))
#define OutputSub ((1 << (OffsetBits - OutputShift)) + (1 << (OffsetBits - OutputShift - 1)))
#define RoundFinal 4
#define DistBits 4
#define PixelMax 1023
#define LocalStride 16

int blend(int src0, int src1, int coef0, int coef1) {
  src0 = (src0 + OutputRoundAdd) >> OutputShift;
  src1 = (src1 + OutputRoundAdd) >> OutputShift;
  int result = (src0 * coef0 + src1 * coef1) >> DistBits;
  result = (result - OutputSub + (1 << (RoundFinal - 1))) >> RoundFinal;
  return clamp(result, 0, PixelMax);
}

groupshared int intermediate_buffer[64 * LocalStride];

[numthreads(64, 1, 1)] void main(uint3 thread
                                 : SV_DispatchThreadID) {
  // A thread behind the last block of the dispatch does nothing, but it takes part in the group syncs: a
  // group sync has to be reached by all threads of the group.
  const bool active = thread.x < cb_wi_count;

  const int wi = thread.x & 1;
  uint4 block = uint4(0, 0, 0, 0);
  [branch] if (active) block = pred_blocks.Load4((cb_pass_offset + (thread.x >> 1)) * 16);
  // block.x - pos xy
  // block.y - flags:
  //        2    plane
  //        3    ref
  //        4    filter_x
  //        4    filter_y
  //        1    skip
  //        3    ref1
  //

  const int plane = block.y & 3;
  const int noskip = block.y & NoSkipFlag;
  int mbx = SubblockW * (block.x & 0xffff);
  int mby = SubblockH * (block.x >> 16);
  const int dx = mbx & (block.y >> (26 - 1)) & 6;
  const int dy = mby & (block.y >> (28 - 1)) & 6;
  int ref_frm = (block.y >> 2) & 7;
  int refplane = ref_frm * 3 + plane;
  int ref_offset = cb_refplanes[refplane].y;
  int ref_stride = cb_refplanes[refplane].x;
  int ref_w = cb_refplanes[refplane].z;
  int ref_h = cb_refplanes[refplane].w;
  int4 scale = cb_scale[ref_frm + 1];
  int mv = block.z;
  int mvx = scale_value(((mbx - dx) << SUBPEL_BITS) + (mv >> 16), scale.x) + SCALE_EXTRA_OFF;
  int mvy = scale_value(((mby - dy) << SUBPEL_BITS) + ((mv << 16) >> 16), scale.z) + SCALE_EXTRA_OFF;
  mvx += (wi + dx) * scale.y;
  mvy += dy * scale.w;
  int x0 = clamp((mvx >> SCALE_SUBPEL_BITS) - 3, -11, ref_w) << 1;
  int y0 = (mvy >> SCALE_SUBPEL_BITS) - 3;
  mvx &= SCALE_SUBPEL_MASK;
  mvy &= SCALE_SUBPEL_MASK;
  int filter_h = (((block.y >> 5) & 15) << 4) + (mvx >> SCALE_EXTRA_BITS);
  int lines = 8 + (((SubblockH - 1) * scale.w + mvy) >> SCALE_SUBPEL_BITS);
  int4 kernel_h0 = cb_kernels[filter_h][0];
  int4 kernel_h1 = cb_kernels[filter_h][1];
  int local_base = (thread.x & 63) * LocalStride;
  int i;
  if (active) for (i = 0; i < lines; ++i) {
    int ref_addr = ref_offset + ref_stride * clamp(y0 + i, 0, ref_h) + x0;
    const uint shift = (ref_addr & 2) * 8;
    ref_addr &= ~3;
    uint4 l = dst_frame.Load4(ref_addr);
    uint l5 = dst_frame.Load(ref_addr + 16);
    l.x = (l.x >> shift) | ((l.y << (24 - shift)) << 8);
    l.y = (l.y >> shift) | ((l.z << (24 - shift)) << 8);
    l.z = (l.z >> shift) | ((l.w << (24 - shift)) << 8);
    l.w = (l.w >> shift) | ((l5 << (24 - shift)) << 8);
    int sum = 0;
    sum += kernel_h0.x * (int)((l.x >> 0) & 0xffff);
    sum += kernel_h0.y * (int)((l.x >> 16) & 0xffff);
    sum += kernel_h0.z * (int)((l.y >> 0) & 0xffff);
    sum += kernel_h0.w * (int)((l.y >> 16) & 0xffff);
    sum += kernel_h1.x * (int)((l.z >> 0) & 0xffff);
    sum += kernel_h1.y * (int)((l.z >> 16) & 0xffff);
    sum += kernel_h1.z * (int)((l.w >> 0) & 0xffff);
    sum += kernel_h1.w * (int)((l.w >> 16) & 0xffff);
    intermediate_buffer[local_base + i] = (sum + FilterLineAdd10bit) >> FilterLineShift;
  }

  // The lines filtered horizontally are handed from the threads that made them to the threads that filter
  // them vertically, through the group memory, once for each of the two references. The threads of a group
  // run in no particular order, so it takes a group sync to have all lines there before any of them is
  // read, and another to have all of them read before the memory is used again. (Upstream had plain group
  // memory barriers, which make no thread wait: that held only where the 64 threads of a group ran in
  // lockstep, as one wave.)
  GroupMemoryBarrierWithGroupSync();

  mvy += wi * scale.w;
  int filter_v = (((block.y >> 9) & 15) << 4) + ((mvy & SCALE_SUBPEL_MASK) >> SCALE_EXTRA_BITS);
  int4 kernel_v0 = cb_kernels[filter_v][0];
  int4 kernel_v1 = cb_kernels[filter_v][1];
  local_base = (mvy >> SCALE_SUBPEL_BITS) + (thread.x & 62) * LocalStride;
  int output[2];
  if (active) for (i = 0; i < 2; ++i) {
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
    output[i] = sum;
  }

  GroupMemoryBarrierWithGroupSync();

  ref_frm = (block.y >> 14) & 7;
  refplane = ref_frm * 3 + plane;
  ref_offset = cb_refplanes[refplane].y;
  ref_stride = cb_refplanes[refplane].x;
  ref_w = cb_refplanes[refplane].z;
  ref_h = cb_refplanes[refplane].w;
  scale = cb_scale[ref_frm + 1];
  mv = block.w;
  mvx = scale_value(((mbx - dx) << SUBPEL_BITS) + (mv >> 16), scale.x) + SCALE_EXTRA_OFF;
  mvy = scale_value(((mby - dy) << SUBPEL_BITS) + ((mv << 16) >> 16), scale.z) + SCALE_EXTRA_OFF;
  mvx += (wi + dx) * scale.y;
  mvy += dy * scale.w;
  x0 = clamp((mvx >> SCALE_SUBPEL_BITS) - 3, -11, ref_w) << 1;
  y0 = (mvy >> SCALE_SUBPEL_BITS) - 3;
  mvx &= SCALE_SUBPEL_MASK;
  mvy &= SCALE_SUBPEL_MASK;
  filter_h = (((block.y >> 5) & 15) << 4) + (mvx >> SCALE_EXTRA_BITS);
  lines = 8 + (((SubblockH - 1) * scale.w + mvy) >> SCALE_SUBPEL_BITS);
  kernel_h0 = cb_kernels[filter_h][0];
  kernel_h1 = cb_kernels[filter_h][1];
  local_base = (thread.x & 63) * LocalStride;

  if (active) for (i = 0; i < lines; ++i) {
    int ref_addr = ref_offset + ref_stride * clamp(y0 + i, 0, ref_h) + x0;
    const uint shift = (ref_addr & 2) * 8;
    ref_addr &= ~3;
    uint4 l = dst_frame.Load4(ref_addr);
    uint l5 = dst_frame.Load(ref_addr + 16);
    l.x = (l.x >> shift) | ((l.y << (24 - shift)) << 8);
    l.y = (l.y >> shift) | ((l.z << (24 - shift)) << 8);
    l.z = (l.z >> shift) | ((l.w << (24 - shift)) << 8);
    l.w = (l.w >> shift) | ((l5 << (24 - shift)) << 8);
    int sum = 0;
    sum += kernel_h0.x * (int)((l.x >> 0) & 0xffff);
    sum += kernel_h0.y * (int)((l.x >> 16) & 0xffff);
    sum += kernel_h0.z * (int)((l.y >> 0) & 0xffff);
    sum += kernel_h0.w * (int)((l.y >> 16) & 0xffff);
    sum += kernel_h1.x * (int)((l.z >> 0) & 0xffff);
    sum += kernel_h1.y * (int)((l.z >> 16) & 0xffff);
    sum += kernel_h1.z * (int)((l.w >> 0) & 0xffff);
    sum += kernel_h1.w * (int)((l.w >> 16) & 0xffff);
    intermediate_buffer[local_base + i] = (sum + FilterLineAdd10bit) >> FilterLineShift;
  }

  GroupMemoryBarrierWithGroupSync();
  if (!active) return;

  mvy += wi * scale.w;
  filter_v = (((block.y >> 9) & 15) << 4) + ((mvy & SCALE_SUBPEL_MASK) >> SCALE_EXTRA_BITS);
  kernel_v0 = cb_kernels[filter_v][0];
  kernel_v1 = cb_kernels[filter_v][1];
  local_base = (mvy >> SCALE_SUBPEL_BITS) + (thread.x & 62) * LocalStride;
  int coef0 = (block.y >> 17) & 15;
  int coef1 = (block.y >> 21) & 15;
  for (i = 0; i < 2; ++i) {
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
    output[i] = blend(output[i], sum, coef0, coef1);
  }

  mbx <<= 1;
  mby += wi;
  if (noskip) {
    const int res_addr = cb_planes[plane].w + mbx + mby * cb_planes[plane].z;
    int r = residuals.Load(res_addr);
    output[0] = clamp(output[0] + ((r << 16) >> 16), 0, PixelMax);
    output[1] = clamp(output[1] + (r >> 16), 0, PixelMax);
  }

  const int output_addr = cb_planes[plane].y + mbx + mby * cb_planes[plane].x;
  dst_frame.Store(output_addr, output[0] | (output[1] << 16));
}
