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
#define OutputShift 7
#define OffsetBits 21
#define OutputRoundAdd ((1 << (OutputShift - 1)) + (1 << OffsetBits))
#define OutputSub ((1 << (OffsetBits - OutputShift)) + (1 << (OffsetBits - OutputShift - 1)))
#define RoundFinal 4
#define DistBits 4
#define PixelMax 1023
#define DiffWTDBase 38
#define DiffWTDRoundAdd (1 << 5)
#define DiffWTDRoundShft (6 + 4)
#define DiffWTDBits 6
#define DiffWTDMax 64
#define LocalStride 20

int compute_mask(int src0, int src1, int inv) {
  int m = clamp(DiffWTDBase + ((abs(src0 - src1) + DiffWTDRoundAdd) >> DiffWTDRoundShft), 0, DiffWTDMax);
  return inv ? DiffWTDMax - m : m;
}

int blend(int src0, int src1, int m) {
  int result = (src0 * m + src1 * (DiffWTDMax - m)) >> DiffWTDBits;
  result = (result - OutputSub + (1 << (RoundFinal - 1))) >> RoundFinal;
  return clamp(result, 0, PixelMax);
}

groupshared int intermediate_buffer[64 * LocalStride];

[numthreads(64, 1, 1)] void main(uint3 thread
                                 : SV_DispatchThreadID) {
  if (thread.x >= cb_wi_count) return;

  const int w_log = cb_width_log2;
  const int h_log = cb_height_log2;
  const int subblock = (thread.x >> 2) & ((1 << (w_log + h_log)) - 1);
  const int block_index = cb_pass_offset + (thread.x >> (w_log + h_log + 2));
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

  const int plane = block.y & 3;
  const int noskip = block.y & NoSkipFlag;
  const int wi = thread.x & 3;
  const int dx = SubblockW * (subblock & ((1 << w_log) - 1));
  const int dy = SubblockH * (subblock >> w_log);
  int mbx = SubblockW * (block.x & 0xffff);
  int mby = SubblockH * (block.x >> 16);

  int ref_frm = (block.y >> 2) & 7;
  int refplane = ref_frm * 3 + plane;
  int ref_offset = cb_refplanes[refplane].y;
  int ref_stride = cb_refplanes[refplane].x;
  int ref_w = cb_refplanes[refplane].z;
  int ref_h = cb_refplanes[refplane].w;
  int4 scale = cb_scale[ref_frm + 1];
  int mv = block.z;
  int mvx = scale_value((mbx << SUBPEL_BITS) + (mv >> 16), scale.x) + SCALE_EXTRA_OFF;
  int mvy = scale_value((mby << SUBPEL_BITS) + ((mv << 16) >> 16), scale.z) + SCALE_EXTRA_OFF;
  mvx += (dx + wi) * scale.y;
  mvy += dy * scale.w;
  int x0 = clamp((mvx >> SCALE_SUBPEL_BITS) - 3, -11, ref_w) << 1;
  int y0 = (mvy >> SCALE_SUBPEL_BITS) - 3;
  mvx &= SCALE_SUBPEL_MASK;
  mvy &= SCALE_SUBPEL_MASK;
  int filter_h = (((block.y >> 5) & 15) << 4) + (mvx >> SCALE_EXTRA_BITS);
  int lines = 8 + ((3 * scale.w + mvy) >> SCALE_SUBPEL_BITS);
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
  int filter_v = (((block.y >> 9) & 15) << 4) + ((mvy & SCALE_SUBPEL_MASK) >> SCALE_EXTRA_BITS);
  int4 kernel_v0 = cb_kernels[filter_v][0];
  int4 kernel_v1 = cb_kernels[filter_v][1];
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
    output[i] = sum;
  }

  GroupMemoryBarrier();

  ref_frm = (block.y >> 14) & 7;
  refplane = ref_frm * 3 + plane;
  ref_offset = cb_refplanes[refplane].y;
  ref_stride = cb_refplanes[refplane].x;
  ref_w = cb_refplanes[refplane].z;
  ref_h = cb_refplanes[refplane].w;
  scale = cb_scale[ref_frm + 1];
  mv = block.w;
  mvx = scale_value((mbx << SUBPEL_BITS) + (mv >> 16), scale.x) + SCALE_EXTRA_OFF;
  mvy = scale_value((mby << SUBPEL_BITS) + ((mv << 16) >> 16), scale.z) + SCALE_EXTRA_OFF;
  mvx += (dx + wi) * scale.y;
  mvy += dy * scale.w;
  x0 = clamp((mvx >> SCALE_SUBPEL_BITS) - 3, -11, ref_w) << 1;
  y0 = (mvy >> SCALE_SUBPEL_BITS) - 3;
  mvx &= SCALE_SUBPEL_MASK;
  mvy &= SCALE_SUBPEL_MASK;
  filter_h = (((block.y >> 5) & 15) << 4) + (mvx >> SCALE_EXTRA_BITS);
  lines = 8 + ((3 * scale.w + mvy) >> SCALE_SUBPEL_BITS);
  kernel_h0 = cb_kernels[filter_h][0];
  kernel_h1 = cb_kernels[filter_h][1];
  local_base = (thread.x & 63) * LocalStride;

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
  filter_v = (((block.y >> 9) & 15) << 4) + ((mvy & SCALE_SUBPEL_MASK) >> SCALE_EXTRA_BITS);
  kernel_v0 = cb_kernels[filter_v][0];
  kernel_v1 = cb_kernels[filter_v][1];
  local_base = (mvy >> SCALE_SUBPEL_BITS) + (thread.x & 60) * LocalStride;
  const int inv = (block.y >> 17) & 1;

  int mask[4];
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

    int src0 = (output[i] + OutputRoundAdd) >> OutputShift;
    int src1 = (sum + OutputRoundAdd) >> OutputShift;
    int m = compute_mask(src0, src1, inv);
    output[i] = blend(src0, src1, m);
    mask[i] = m;
  }
  int m0 = mask[0] + mask[1];
  int m1 = mask[2] + mask[3];

  mbx = (mbx + dx) << 1;
  mby += dy + wi;
  if (noskip) {
    const int res_addr = cb_planes[plane].w + mbx + mby * cb_planes[plane].z;
    int2 r = (int2)residuals.Load2(res_addr);
    output[0] = clamp(output[0] + ((r.x << 16) >> 16), 0, PixelMax);
    output[1] = clamp(output[1] + (r.x >> 16), 0, PixelMax);
    output[2] = clamp(output[2] + ((r.y << 16) >> 16), 0, PixelMax);
    output[3] = clamp(output[3] + (r.y >> 16), 0, PixelMax);
  }

  const int output_addr = cb_planes[plane].y + mbx + mby * cb_planes[plane].x;
  dst_frame.Store2(output_addr, uint2(output[0] | (output[1] << 16), output[2] | (output[3] << 16)));

  GroupMemoryBarrier();

  local_base = (thread.x & 63) << 1;
  intermediate_buffer[local_base + 0] = m0;
  intermediate_buffer[local_base + 1] = m1;

  GroupMemoryBarrier();

  // even lines of every even 4x4 subblock
  if ((thread.x & 5) == 0) {
    m0 = (m0 + intermediate_buffer[local_base + 2] + 2) >> 2;
    m1 = (m1 + intermediate_buffer[local_base + 3] + 2) >> 2;
    // mask from the next 4x4 block:
    int m2 = (intermediate_buffer[local_base + 8] + intermediate_buffer[local_base + 10] + 2) >> 2;
    int m3 = (intermediate_buffer[local_base + 9] + intermediate_buffer[local_base + 11] + 2) >> 2;
    int chroma_offset = (mbx + mby * cb_planes[1].x) >> 1;
    uint chroma_mask = m0 | (m1 << 8) | (m2 << 16) | (m3 << 24);
    dst_frame.Store(cb_planes[1].y + chroma_offset, chroma_mask);
    dst_frame.Store(cb_planes[2].y + chroma_offset, chroma_mask);
  }
}
