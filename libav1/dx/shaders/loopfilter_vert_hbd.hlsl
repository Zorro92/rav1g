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
#define SignedPixelMin -512
#define SignedPixelMax 511
#define PixelGray 512
#define Thresh10 4

groupshared uint input_edges[16 * (64 / 4)];

void filter_vertical_edge(RWByteAddressBuffer buffer, uint offset, int4 limits, uint type) {
  uint2 pixels0 = uint2(0, 0);
  uint2 pixels2 = uint2(0, 0);

  if (type == 3) pixels0 = buffer.Load2(offset - 16);
  uint4 pixels = buffer.Load4(offset - 8);
  if (type == 3) pixels2 = buffer.Load2(offset + 8);

  int4 p0, p1, q0, q1;
  p0.x = (pixels0.x >> 0) & 0xffff;
  p0.y = (pixels0.x >> 16) & 0xffff;
  p0.z = (pixels0.y >> 0) & 0xffff;
  p0.w = (pixels0.y >> 16) & 0xffff;

  p1.x = (pixels.x >> 0) & 0xffff;
  p1.y = (pixels.x >> 16) & 0xffff;
  p1.z = (pixels.y >> 0) & 0xffff;
  p1.w = (pixels.y >> 16) & 0xffff;

  q0.x = (pixels.z >> 0) & 0xffff;
  q0.y = (pixels.z >> 16) & 0xffff;
  q0.z = (pixels.w >> 0) & 0xffff;
  q0.w = (pixels.w >> 16) & 0xffff;

  q1.x = (pixels2.x >> 0) & 0xffff;
  q1.y = (pixels2.x >> 16) & 0xffff;
  q1.z = (pixels2.y >> 0) & 0xffff;
  q1.w = (pixels2.y >> 16) & 0xffff;

  int mask = abs(p1.z - p1.w) <= limits.x && abs(q0.x - q0.y) <= limits.x &&
             abs(p1.w - q0.x) * 2 + (abs(p1.z - q0.y) >> 1) <= limits.y;
  mask &= (abs(p1.y - p1.z) <= limits.x && abs(q0.y - q0.z) <= limits.x) | (type == 0);
  mask &= (abs(p1.x - p1.y) <= limits.x && abs(q0.z - q0.w) <= limits.x) | (type <= 1);

  if (!mask) return;

  int flat_uv = abs(p1.y - p1.w) <= Thresh10 && abs(p1.z - p1.w) <= Thresh10 && abs(q0.y - q0.x) <= Thresh10 &&
                abs(q0.z - q0.x) <= Thresh10;
  int flat = flat_uv && abs(p1.x - p1.w) <= Thresh10 && abs(q0.w - q0.x) <= Thresh10;
  int flat2 = abs(p0.y - p1.w) <= Thresh10 && abs(p0.z - p1.w) <= Thresh10 && abs(p0.w - p1.w) <= Thresh10 &&
              abs(q1.x - q0.x) <= Thresh10 && abs(q1.y - q0.x) <= Thresh10 && abs(q1.z - q0.x) <= Thresh10;
  if (type == 3 && flat && flat2) {
    uint pix_l;
    uint pix_r;
    pix_l = ((p0.y * 7 + p0.z * 2 + p0.w * 2 + p1.x * 1 + p1.y * 1 + p1.z * 1 + p1.w * 1 + q0.x * 1 + 8) >> 4);
    pix_l |=
        ((p0.y * 5 + p0.z * 2 + p0.w * 2 + p1.x * 2 + p1.y * 1 + p1.z * 1 + p1.w * 1 + q0.x * 1 + q0.y * 1 + 8) >> 4)
        << 16;
    pixels.x = ((p0.y * 4 + p0.z * 1 + p0.w * 2 + p1.x * 2 + p1.y * 2 + p1.z * 1 + p1.w * 1 + q0.x * 1 + q0.y * 1 +
                 q0.z * 1 + 8) >>
                4);
    pixels.x |= ((p0.y * 3 + p0.z * 1 + p0.w * 1 + p1.x * 2 + p1.y * 2 + p1.z * 2 + p1.w * 1 + q0.x * 1 + q0.y * 1 +
                  q0.z * 1 + q0.w * 1 + 8) >>
                 4)
                << 16;
    pixels.y = ((p0.y * 2 + p0.z * 1 + p0.w * 1 + p1.x * 1 + p1.y * 2 + p1.z * 2 + p1.w * 2 + q0.x * 1 + q0.y * 1 +
                 q0.z * 1 + q0.w * 1 + q1.x * 1 + 8) >>
                4);
    pixels.y |= ((p0.y * 1 + p0.z * 1 + p0.w * 1 + p1.x * 1 + p1.y * 1 + p1.z * 2 + p1.w * 2 + q0.x * 2 + q0.y * 1 +
                  q0.z * 1 + q0.w * 1 + q1.x * 1 + q1.y * 1 + 8) >>
                 4)
                << 16;
    pixels.z = ((p0.z * 1 + p0.w * 1 + p1.x * 1 + p1.y * 1 + p1.z * 1 + p1.w * 2 + q0.x * 2 + q0.y * 2 + q0.z * 1 +
                 q0.w * 1 + q1.x * 1 + q1.y * 1 + q1.z * 1 + 8) >>
                4);
    pixels.z |= ((p0.w * 1 + p1.x * 1 + p1.y * 1 + p1.z * 1 + p1.w * 1 + q0.x * 2 + q0.y * 2 + q0.z * 2 + q0.w * 1 +
                  q1.x * 1 + q1.y * 1 + q1.z * 2 + 8) >>
                 4)
                << 16;
    pixels.w = ((p1.x * 1 + p1.y * 1 + p1.z * 1 + p1.w * 1 + q0.x * 1 + q0.y * 2 + q0.z * 2 + q0.w * 2 + q1.x * 1 +
                 q1.y * 1 + q1.z * 3 + 8) >>
                4);
    pixels.w |= ((p1.y * 1 + p1.z * 1 + p1.w * 1 + q0.x * 1 + q0.y * 1 + q0.z * 2 + q0.w * 2 + q1.x * 2 + q1.y * 1 +
                  q1.z * 4 + 8) >>
                 4)
                << 16;
    pix_r =
        ((p1.z * 1 + p1.w * 1 + q0.x * 1 + q0.y * 1 + q0.z * 1 + q0.w * 2 + q1.x * 2 + q1.y * 2 + q1.z * 5 + 8) >> 4);
    pix_r |= ((p1.w * 1 + q0.x * 1 + q0.y * 1 + q0.z * 1 + q0.w * 1 + q1.x * 2 + q1.y * 2 + q1.z * 7 + 8) >> 4) << 16;
    buffer.Store(offset - 12, pix_l);
    buffer.Store4(offset - 8, pixels);
    buffer.Store(offset + 8, pix_r);
  } else if (type >= 2 && flat) {
    int v = p1.x * 3 + p1.y + p1.z + p1.w + q0.x;
    pixels.x = p1.x;
    pixels.x |= ((v + p1.y + 4) >> 3) << 16;
    v += -p1.x + q0.y;
    pixels.y = ((v + p1.z + 4) >> 3);
    v += -p1.x + q0.z;
    pixels.y |= ((v + p1.w + 4) >> 3) << 16;

    v += -p1.x + q0.w;
    pixels.z = (v + q0.x + 4) >> 3;
    v += -p1.y + q0.w;
    pixels.z |= ((v + q0.y + 4) >> 3) << 16;
    v += -p1.z + q0.w;
    pixels.w = ((v + q0.z + 4) >> 3);
    pixels.w |= q0.w << 16;
    buffer.Store4(offset - 8, pixels);
  } else if (type == 1 && flat_uv) {
    // 5-tap filter [1, 2, 2, 2, 1]
    pixels.y = ((p1.y * 3 + p1.z * 2 + p1.w * 2 + q0.x + 4) >> 3);
    pixels.y |= ((p1.y + p1.z * 2 + p1.w * 2 + q0.x * 2 + q0.y + 4) >> 3) << 16;
    pixels.z = ((p1.z + p1.w * 2 + q0.x * 2 + q0.y * 2 + q0.z + 4) >> 3);
    pixels.z |= ((p1.w + q0.x * 2 + q0.y * 2 + q0.z * 3 + 4) >> 3) << 16;
    buffer.Store2(offset - 4, pixels.yz);
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

    pixels.y = (clamp(ps0 + f2, SignedPixelMin, SignedPixelMax) + PixelGray) << 16;
    pixels.z = clamp(qs0 - f1, SignedPixelMin, SignedPixelMax) + PixelGray;

    f0 = ((f1 + 1) >> 1) & (~hev);

    pixels.y |= clamp(ps1 + f0, SignedPixelMin, SignedPixelMax) + PixelGray;
    pixels.z |= (clamp(qs1 - f0, SignedPixelMin, SignedPixelMax) + PixelGray) << 16;
    buffer.Store2(offset - 4, pixels.yz);
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
  // A thread behind the last block of the dispatch does nothing, but it takes part in the group syncs: a
  // group sync has to be reached by all threads of the group.
  const bool active = thread.x < cb_wicount;
  const int plane = cb_plane;
  const int wi = thread.x & 3;
  const uint block_id = ((thread.x >> 2) << 1) + cb_block_id_offset;
  const int local_offset = (thread.x & (64 - 4)) << 2;
  if (active) {
    uint2 data = lf_blocks.Load2((cb_offset_base + block_id) * BlockSize + (wi << 3));
    input_edges[local_offset + (wi << 2) + 0] = data.x & 0xffff;
    input_edges[local_offset + (wi << 2) + 1] = (data.x >> 16);
    input_edges[local_offset + (wi << 2) + 2] = data.y & 0xffff;
    input_edges[local_offset + (wi << 2) + 3] = (data.y >> 16);
  }

  // The sixteen edges of a block are loaded by its four threads, four each, and every thread walks all of
  // them. The threads of a group run in no particular order, so it takes a group sync to have the edges
  // there before they are read. (Upstream had a plain group memory barrier, which makes no thread wait:
  // that held only where the 64 threads of a group ran in lockstep, as one wave.)
  GroupMemoryBarrierWithGroupSync();

  const int block_x = block_id % cb_block_cols;
  const int block_y = block_id / cb_block_cols;
  const int stride = cb_planes[plane].x;
  const int addr = cb_planes[plane].y + block_x * 128 + (block_y * 4 + wi) * stride;

  if (!active) return;

  for (int col = 0; col < 16;) {
    uint edge = input_edges[local_offset + col];
    if (!edge) break;
    int level = edge & 63;
    int filter = (edge >> 6) & 3;
    int step = edge >> 8;

    if (level) {
      const int4 limits = cb_limits[level] << 2;
      filter_vertical_edge(dst_frame, addr + col * 8, limits, filter);
    }
    col += step;
  }
}
