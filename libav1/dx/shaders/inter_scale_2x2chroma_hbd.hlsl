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
#define OffsetBits 21
#define OutputRoundAdd ((1 << (OutputShift - 1)) + (1 << OffsetBits))
#define OutputSub ((1 << (OffsetBits - OutputShift)) + (1 << (OffsetBits - OutputShift - 1)))
#define PixelMax 1023
#define LocalStride 16

groupshared int intermediate_buffer[64 * LocalStride];

[numthreads(64, 1, 1)] void main(uint3 thread
                                 : SV_DispatchThreadID) {
  if (thread.x >= cb_wi_count) return;

  uint4 block = pred_blocks.Load4((cb_pass_offset + (thread.x >> 1)) << 4);
  const int wi = thread.x & 1;

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
  const int dx = mbx & (block.y >> (26 - 1)) & 6;
  const int dy = mby & (block.y >> (28 - 1)) & 6;

  int mv = block.z;
  int mvx = scale_value(((mbx - dx) << SUBPEL_BITS) + (mv >> 16), scale.x) + SCALE_EXTRA_OFF;
  int mvy = scale_value(((mby - dy) << SUBPEL_BITS) + ((mv << 16) >> 16), scale.z) + SCALE_EXTRA_OFF;
  mvx += (wi + dx) * scale.y;
  mvy += dy * scale.w;
  int x0 = clamp((mvx >> SCALE_SUBPEL_BITS) - 3, -11, ref_w) << 1;
  int y0 = (mvy >> SCALE_SUBPEL_BITS) - 3;
  mvx &= SCALE_SUBPEL_MASK;
  mvy &= SCALE_SUBPEL_MASK;

  mby += wi;

  const int filter_h = (((block.y >> 5) & 15) << 4) + (mvx >> SCALE_EXTRA_BITS);
  const int lines = 8 + (((SubblockH - 1) * scale.w + mvy) >> SCALE_SUBPEL_BITS);

  int4 kernel_h0 = cb_kernels[filter_h][0];
  int4 kernel_h1 = cb_kernels[filter_h][1];
  int local_base = (thread.x & 63) * LocalStride;
  int i;
  for (i = 0; i < lines; ++i) {
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

  GroupMemoryBarrier();

  mvy += wi * scale.w;
  const int filter_v = (((block.y >> 9) & 15) << 4) + ((mvy & SCALE_SUBPEL_MASK) >> SCALE_EXTRA_BITS);
  const int4 kernel_v0 = cb_kernels[filter_v][0];
  const int4 kernel_v1 = cb_kernels[filter_v][1];
  local_base = (mvy >> SCALE_SUBPEL_BITS) + (thread.x & 62) * LocalStride;
  int output[2];
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
    output[i] = clamp((int)(((sum + OutputRoundAdd) >> OutputShift) - OutputSub), 0, PixelMax);
  }

  mbx <<= 1;
  if (noskip) {
    const int res_addr = cb_planes[plane].w + mbx + mby * cb_planes[plane].z;
    int r = residuals.Load(res_addr);
    output[0] = clamp(output[0] + ((r << 16) >> 16), 0, PixelMax);
    output[1] = clamp(output[1] + (r >> 16), 0, PixelMax);
  }

  const int output_addr = cb_planes[plane].y + mbx + mby * cb_planes[plane].x;
  dst_frame.Store(output_addr, output[0] | (output[1] << 16));
}
