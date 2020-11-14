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

#include "idct_shader_common.h"

#define ScanSize 4
#define QuantShift 2

cbuffer cb_scans_4x4 : register(b0) { int4 cb_scans[ScanSize * 3]; };

groupshared int shared_4x4_mem[4 * 64];

[numthreads(64, 1, 1)] void main(uint3 thread
                                 : SV_DispatchThreadID) {
  if (thread.x >= cb_wicount) return;
  int wi = thread.x & 3;
  uint4 block = buf_blocks.Load4((cb_index_offset + thread.x / 4) * IdctBlockSize);
  const int plane = (block.x >> 21) & 3;
  const int loc_mem_offset = 4 * (thread.x & (64 - 4));
  const int scan_offset = ScanSize * ((block.x >> ScanShift) & ScanMask) + wi;
  const int coef_count = block.x & 0x7ff;

  const int input_offset = (block.y + wi * 4) << 2;
  int4 coefs = (wi < coef_count) ? (int4)buf_input.Load4(input_offset) : int4(0, 0, 0, 0);

  shared_4x4_mem[loc_mem_offset + cb_scans[scan_offset].x] = coefs.x;
  shared_4x4_mem[loc_mem_offset + cb_scans[scan_offset].y] = coefs.y;
  shared_4x4_mem[loc_mem_offset + cb_scans[scan_offset].z] = coefs.z;
  shared_4x4_mem[loc_mem_offset + cb_scans[scan_offset].w] = coefs.w;

  GroupMemoryBarrier();

  const int loc_offset_row = (thread.x & 63) * 4;
  const int loc_offset_col = loc_mem_offset + wi;

  int a1, b1, c1, d1, e1;
  a1 = shared_4x4_mem[loc_offset_row + 0] >> QuantShift;
  c1 = shared_4x4_mem[loc_offset_row + 1] >> QuantShift;
  d1 = shared_4x4_mem[loc_offset_row + 2] >> QuantShift;
  b1 = shared_4x4_mem[loc_offset_row + 3] >> QuantShift;

  a1 += c1;
  d1 -= b1;
  e1 = (a1 - d1) >> 1;
  b1 = e1 - b1;
  c1 = e1 - c1;
  a1 -= b1;
  d1 += c1;

  shared_4x4_mem[loc_offset_row + 0] = a1;
  shared_4x4_mem[loc_offset_row + 1] = b1;
  shared_4x4_mem[loc_offset_row + 2] = c1;
  shared_4x4_mem[loc_offset_row + 3] = d1;

  GroupMemoryBarrier();

  a1 = shared_4x4_mem[loc_offset_col + 0];
  c1 = shared_4x4_mem[loc_offset_col + 4];
  d1 = shared_4x4_mem[loc_offset_col + 8];
  b1 = shared_4x4_mem[loc_offset_col + 12];
  a1 += c1;
  d1 -= b1;
  e1 = (a1 - d1) >> 1;
  b1 = e1 - b1;
  c1 = e1 - c1;
  a1 -= b1;
  d1 += c1;
  shared_4x4_mem[loc_offset_col + 0] = a1;
  shared_4x4_mem[loc_offset_col + 4] = b1;
  shared_4x4_mem[loc_offset_col + 8] = c1;
  shared_4x4_mem[loc_offset_col + 12] = d1;

  GroupMemoryBarrier();

  const int stride = cb_planes[plane].x;
  const int offset =
      cb_planes[plane].y + 8 * ((block.z & 0xffff) + (wi >> 2)) + (4 * (block.z >> 16) + (wi & 3)) * stride;

  const int output_offset = loc_mem_offset + (wi & 3) * 4 + 4 * (wi >> 2);
  buf_dst.Store2(offset,
                 int2((shared_4x4_mem[output_offset + 0] & 0xffff) | (shared_4x4_mem[output_offset + 1] << 16),
                      (shared_4x4_mem[output_offset + 2] & 0xffff) | (shared_4x4_mem[output_offset + 3] << 16)));
}
