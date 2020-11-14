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

#define BlockSize 32
#define Stride64 64
#define SignedPixelMin -512
#define SignedPixelMax 511
#define PixelGray 512
#define Thresh10 4

groupshared uint input_edges[16 * (64 / 4)];
groupshared uint shared_mem[64 * 16];

void filter_horizontal_edge(RWByteAddressBuffer buffer, uint offset, uint stride, int4 limits, uint type, int wi) {
  const int shift = (offset & 2) * 8;
  offset &= ~3;
  int4 p0 = int4(0, 0, 0, 0);
  int4 q1 = int4(0, 0, 0, 0);
  int4 p1, q0;
  if (type == 3) {
    p0.x = (buffer.Load(offset - 8 * stride) >> shift) & 0xffff;
    p0.y = (buffer.Load(offset - 7 * stride) >> shift) & 0xffff;
    p0.z = (buffer.Load(offset - 6 * stride) >> shift) & 0xffff;
    p0.w = (buffer.Load(offset - 5 * stride) >> shift) & 0xffff;
  }
  p1.x = (buffer.Load(offset - 4 * stride) >> shift) & 0xffff;
  p1.y = (buffer.Load(offset - 3 * stride) >> shift) & 0xffff;
  p1.z = (buffer.Load(offset - 2 * stride) >> shift) & 0xffff;
  p1.w = (buffer.Load(offset - 1 * stride) >> shift) & 0xffff;
  q0.x = (buffer.Load(offset) >> shift) & 0xffff;
  q0.y = (buffer.Load(offset + 1 * stride) >> shift) & 0xffff;
  q0.z = (buffer.Load(offset + 2 * stride) >> shift) & 0xffff;
  q0.w = (buffer.Load(offset + 3 * stride) >> shift) & 0xffff;

  if (type == 3) {
    q1.x = (buffer.Load(offset + 4 * stride) >> shift) & 0xffff;
    q1.y = (buffer.Load(offset + 5 * stride) >> shift) & 0xffff;
    q1.z = (buffer.Load(offset + 6 * stride) >> shift) & 0xffff;
    q1.w = (buffer.Load(offset + 7 * stride) >> shift) & 0xffff;
  }

  shared_mem[Stride64 * 0 + wi] = p0.x;
  shared_mem[Stride64 * 1 + wi] = p0.y;
  shared_mem[Stride64 * 2 + wi] = p0.z;
  shared_mem[Stride64 * 3 + wi] = p0.w;
  shared_mem[Stride64 * 4 + wi] = p1.x;
  shared_mem[Stride64 * 5 + wi] = p1.y;
  shared_mem[Stride64 * 6 + wi] = p1.z;
  shared_mem[Stride64 * 7 + wi] = p1.w;
  shared_mem[Stride64 * 8 + wi] = q0.x;
  shared_mem[Stride64 * 9 + wi] = q0.y;
  shared_mem[Stride64 * 10 + wi] = q0.z;
  shared_mem[Stride64 * 11 + wi] = q0.w;
  shared_mem[Stride64 * 12 + wi] = q1.x;
  shared_mem[Stride64 * 13 + wi] = q1.y;
  shared_mem[Stride64 * 14 + wi] = q1.z;
  shared_mem[Stride64 * 15 + wi] = q1.w;

  int mask = abs(p1.z - p1.w) <= limits.x && abs(q0.x - q0.y) <= limits.x &&
             abs(p1.w - q0.x) * 2 + (abs(p1.z - q0.y) >> 1) <= limits.y;

  mask &= (abs(p1.y - p1.z) <= limits.x && abs(q0.y - q0.z) <= limits.x) | (type == 0);
  mask &= (abs(p1.x - p1.y) <= limits.x && abs(q0.z - q0.w) <= limits.x) | (type <= 1);

  if (mask) {
    int flat_uv = abs(p1.y - p1.w) <= Thresh10 && abs(p1.z - p1.w) <= Thresh10 && abs(q0.y - q0.x) <= Thresh10 &&
                  abs(q0.z - q0.x) <= Thresh10;
    int flat = flat_uv && abs(p1.x - p1.w) <= Thresh10 && abs(q0.w - q0.x) <= Thresh10;
    int flat2 = abs(p0.y - p1.w) <= Thresh10 && abs(p0.z - p1.w) <= Thresh10 && abs(p0.w - p1.w) <= Thresh10 &&
                abs(q1.x - q0.x) <= Thresh10 && abs(q1.y - q0.x) <= Thresh10 && abs(q1.z - q0.x) <= Thresh10;

    if (type == 3 && flat && flat2) {
      shared_mem[Stride64 * 2 + wi] =
          (p0.y * 7 + p0.z * 2 + p0.w * 2 + p1.x * 1 + p1.y * 1 + p1.z * 1 + p1.w * 1 + q0.x * 1 + 8) >> 4;
      shared_mem[Stride64 * 3 + wi] =
          (p0.y * 5 + p0.z * 2 + p0.w * 2 + p1.x * 2 + p1.y * 1 + p1.z * 1 + p1.w * 1 + q0.x * 1 + q0.y * 1 + 8) >> 4;
      shared_mem[Stride64 * 4 + wi] = (p0.y * 4 + p0.z * 1 + p0.w * 2 + p1.x * 2 + p1.y * 2 + p1.z * 1 + p1.w * 1 +
                                       q0.x * 1 + q0.y * 1 + q0.z * 1 + 8) >>
                                      4;
      shared_mem[Stride64 * 5 + wi] = (p0.y * 3 + p0.z * 1 + p0.w * 1 + p1.x * 2 + p1.y * 2 + p1.z * 2 + p1.w * 1 +
                                       q0.x * 1 + q0.y * 1 + q0.z * 1 + q0.w * 1 + 8) >>
                                      4;
      shared_mem[Stride64 * 6 + wi] = (p0.y * 2 + p0.z * 1 + p0.w * 1 + p1.x * 1 + p1.y * 2 + p1.z * 2 + p1.w * 2 +
                                       q0.x * 1 + q0.y * 1 + q0.z * 1 + q0.w * 1 + q1.x * 1 + 8) >>
                                      4;
      shared_mem[Stride64 * 7 + wi] = (p0.y * 1 + p0.z * 1 + p0.w * 1 + p1.x * 1 + p1.y * 1 + p1.z * 2 + p1.w * 2 +
                                       q0.x * 2 + q0.y * 1 + q0.z * 1 + q0.w * 1 + q1.x * 1 + q1.y * 1 + 8) >>
                                      4;
      shared_mem[Stride64 * 8 + wi] = (p0.z * 1 + p0.w * 1 + p1.x * 1 + p1.y * 1 + p1.z * 1 + p1.w * 2 + q0.x * 2 +
                                       q0.y * 2 + q0.z * 1 + q0.w * 1 + q1.x * 1 + q1.y * 1 + q1.z * 1 + 8) >>
                                      4;
      shared_mem[Stride64 * 9 + wi] = (p0.w * 1 + p1.x * 1 + p1.y * 1 + p1.z * 1 + p1.w * 1 + q0.x * 2 + q0.y * 2 +
                                       q0.z * 2 + q0.w * 1 + q1.x * 1 + q1.y * 1 + q1.z * 2 + 8) >>
                                      4;
      shared_mem[Stride64 * 10 + wi] = (p1.x * 1 + p1.y * 1 + p1.z * 1 + p1.w * 1 + q0.x * 1 + q0.y * 2 + q0.z * 2 +
                                        q0.w * 2 + q1.x * 1 + q1.y * 1 + q1.z * 3 + 8) >>
                                       4;
      shared_mem[Stride64 * 11 + wi] = (p1.y * 1 + p1.z * 1 + p1.w * 1 + q0.x * 1 + q0.y * 1 + q0.z * 2 + q0.w * 2 +
                                        q1.x * 2 + q1.y * 1 + q1.z * 4 + 8) >>
                                       4;
      shared_mem[Stride64 * 12 + wi] =
          (p1.z * 1 + p1.w * 1 + q0.x * 1 + q0.y * 1 + q0.z * 1 + q0.w * 2 + q1.x * 2 + q1.y * 2 + q1.z * 5 + 8) >> 4;
      shared_mem[Stride64 * 13 + wi] =
          (p1.w * 1 + q0.x * 1 + q0.y * 1 + q0.z * 1 + q0.w * 1 + q1.x * 2 + q1.y * 2 + q1.z * 7 + 8) >> 4;
    } else if (type >= 2 && flat) {
      int v = p1.x * 3 + p1.y + p1.z + p1.w + q0.x;
      shared_mem[Stride64 * 5 + wi] = (v + p1.y + 4) >> 3;
      v += -p1.x + q0.y;
      shared_mem[Stride64 * 6 + wi] = (v + p1.z + 4) >> 3;
      v += -p1.x + q0.z;
      shared_mem[Stride64 * 7 + wi] = (v + p1.w + 4) >> 3;
      v += -p1.x + q0.w;
      shared_mem[Stride64 * 8 + wi] = (v + q0.x + 4) >> 3;
      v += -p1.y + q0.w;
      shared_mem[Stride64 * 9 + wi] = (v + q0.y + 4) >> 3;
      v += -p1.z + q0.w;
      shared_mem[Stride64 * 10 + wi] = (v + q0.z + 4) >> 3;
    } else if (type == 1 && flat_uv) {
      // 5-tap filter [1, 2, 2, 2, 1]
      shared_mem[Stride64 * 6 + wi] = (p1.y * 3 + p1.z * 2 + p1.w * 2 + q0.x + 4) >> 3;
      shared_mem[Stride64 * 7 + wi] = (p1.y + p1.z * 2 + p1.w * 2 + q0.x * 2 + q0.y + 4) >> 3;
      shared_mem[Stride64 * 8 + wi] = (p1.z + p1.w * 2 + q0.x * 2 + q0.y * 2 + q0.z + 4) >> 3;
      shared_mem[Stride64 * 9 + wi] = (p1.w + q0.x * 2 + q0.y * 2 + q0.z * 3 + 4) >> 3;
    } else {
      uint hev = (abs(p1.w - p1.z) > limits.z || abs(q0.x - q0.y) > limits.z) ? 0xffffffff : 0;
      int ps1 = p1.z - PixelGray;
      int ps0 = p1.w - PixelGray;
      int qs0 = q0.x - PixelGray;
      int qs1 = q0.y - PixelGray;

      int f0 = clamp(ps1 - qs1, SignedPixelMin, SignedPixelMax) & hev;
      f0 = clamp(f0 + 3 * (qs0 - ps0), SignedPixelMin, SignedPixelMax);
      int f1 = clamp(f0 + 4, SignedPixelMin, SignedPixelMax) >> 3;
      int f2 = clamp(f0 + 3, SignedPixelMin, SignedPixelMax) >> 3;

      shared_mem[Stride64 * 7 + wi] = clamp(ps0 + f2, SignedPixelMin, SignedPixelMax) + PixelGray;
      shared_mem[Stride64 * 8 + wi] = (clamp(qs0 - f1, SignedPixelMin, SignedPixelMax) + PixelGray);

      f0 = ((f1 + 1) >> 1) & (~hev);

      shared_mem[Stride64 * 6 + wi] = clamp(ps1 + f0, SignedPixelMin, SignedPixelMax) + PixelGray;
      shared_mem[Stride64 * 9 + wi] = clamp(qs1 - f0, SignedPixelMin, SignedPixelMax) + PixelGray;
    }
  }

  GroupMemoryBarrier();

  const int wi4 = wi & 3;
  const int base = (wi - wi4) + wi4 * Stride64;
  const int dst_offset = (offset & (~7)) + wi4 * stride;

  if (type == 3 && wi4 >= 2) {
    buffer.Store2(dst_offset - 8 * stride,
                  uint2(shared_mem[base + Stride64 * 0 + 0] | shared_mem[base + Stride64 * 0 + 1] << 16,
                        shared_mem[base + Stride64 * 0 + 2] | shared_mem[base + Stride64 * 0 + 3] << 16));
  }
  if (type > 1 || wi4 >= 2) {
    buffer.Store2(dst_offset - 4 * stride,
                  uint2(shared_mem[base + Stride64 * 4 + 0] | shared_mem[base + Stride64 * 4 + 1] << 16,
                        shared_mem[base + Stride64 * 4 + 2] | shared_mem[base + Stride64 * 4 + 3] << 16));
  }
  if (type > 1 || wi4 < 2) {
    buffer.Store2(dst_offset - 0 * stride,
                  uint2(shared_mem[base + Stride64 * 8 + 0] | shared_mem[base + Stride64 * 8 + 1] << 16,
                        shared_mem[base + Stride64 * 8 + 2] | shared_mem[base + Stride64 * 8 + 3] << 16));
  }
  if (type == 3 && wi4 < 2) {
    buffer.Store2(dst_offset + 4 * stride,
                  uint2(shared_mem[base + Stride64 * 12 + 0] | shared_mem[base + Stride64 * 12 + 1] << 16,
                        shared_mem[base + Stride64 * 12 + 2] | shared_mem[base + Stride64 * 12 + 3] << 16));
  }
}

RWByteAddressBuffer dst_frame : register(u0);
ByteAddressBuffer lf_blocks : register(t0);

cbuffer LoopfilterData : register(b0) {
  int4 cb_planes[3];
  int4 cb_limits[64];
};

cbuffer LoopfilterSRT : register(b1) {
  uint cb_wicount;
  uint cb_plane;
  uint cb_offset_base;
  uint cb_block_cols;
  uint cb_block_id_offset;
};

[numthreads(64, 1, 1)] void main(uint3 thread
                                 : SV_DispatchThreadID) {
  if (thread.x >= cb_wicount) return;
  const int plane = cb_plane;
  const int wi = thread.x & 3;
  const uint block_id = thread.x >> 2;
  uint2 data = lf_blocks.Load2((cb_offset_base + block_id) * BlockSize + (wi << 3));
  const int local_offset = (thread.x & (64 - 4)) << 2;
  input_edges[local_offset + (wi << 2) + 0] = data.x & 0xffff;
  input_edges[local_offset + (wi << 2) + 1] = (data.x >> 16);
  input_edges[local_offset + (wi << 2) + 2] = data.y & 0xffff;
  input_edges[local_offset + (wi << 2) + 3] = (data.y >> 16);

  GroupMemoryBarrier();

  const int block_x = block_id % cb_block_cols;
  const int block_y = block_id / cb_block_cols;
  const int stride = cb_planes[plane].x;
  const int addr = cb_planes[plane].y + block_x * 8 + wi * 2 + block_y * 64 * stride;

  for (int row = 0; row < 16;) {
    uint edge = input_edges[local_offset + row];
    if (!edge) break;
    int level = edge & 63;
    int filter = (edge >> 6) & 3;
    int step = edge >> 8;

    if (level) {
      const int4 limits = cb_limits[level] << 2;
      filter_horizontal_edge(dst_frame, addr + row * 4 * stride, stride, limits, filter, thread.x & 63);
    }
    row += step;
  }
}
