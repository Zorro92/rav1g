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

groupshared uint input_edges[16 * (64 / 4)];

void filter_vertical_edge(RWByteAddressBuffer buffer, uint offset, int4 limits, uint type) {
  uint4 pixels = buffer.Load4(offset - 8);
  int4 p0, p1, q0, q1;
  p0.x = (pixels.x >> 0) & 0xff;
  p0.y = (pixels.x >> 8) & 0xff;
  p0.z = (pixels.x >> 16) & 0xff;
  p0.w = (pixels.x >> 24) & 0xff;

  p1.x = (pixels.y >> 0) & 0xff;
  p1.y = (pixels.y >> 8) & 0xff;
  p1.z = (pixels.y >> 16) & 0xff;
  p1.w = (pixels.y >> 24) & 0xff;

  q0.x = (pixels.z >> 0) & 0xff;
  q0.y = (pixels.z >> 8) & 0xff;
  q0.z = (pixels.z >> 16) & 0xff;
  q0.w = (pixels.z >> 24) & 0xff;

  q1.x = (pixels.w >> 0) & 0xff;
  q1.y = (pixels.w >> 8) & 0xff;
  q1.z = (pixels.w >> 16) & 0xff;
  q1.w = (pixels.w >> 24) & 0xff;

  int mask = abs(p1.z - p1.w) <= limits.x && abs(q0.x - q0.y) <= limits.x &&
             abs(p1.w - q0.x) * 2 + (abs(p1.z - q0.y) >> 1) <= limits.y;
  mask &= (abs(p1.y - p1.z) <= limits.x && abs(q0.y - q0.z) <= limits.x) | (type == 0);
  mask &= (abs(p1.x - p1.y) <= limits.x && abs(q0.z - q0.w) <= limits.x) | (type <= 1);

  if (!mask) return;

  int flat_uv = abs(p1.y - p1.w) <= 1 && abs(p1.z - p1.w) <= 1 && abs(q0.y - q0.x) <= 1 && abs(q0.z - q0.x) <= 1;
  int flat = flat_uv && abs(p1.x - p1.w) <= 1 && abs(q0.w - q0.x) <= 1;
  int flat2 = abs(p0.y - p1.w) <= 1 && abs(p0.z - p1.w) <= 1 && abs(p0.w - p1.w) <= 1 && abs(q1.x - q0.x) <= 1 &&
              abs(q1.y - q0.x) <= 1 && abs(q1.z - q0.x) <= 1;

  if (type == 3 && flat && flat2) {
    pixels.x &= 0x0000ffff;
    pixels.w &= 0xffff0000;
    pixels.x |= ((p0.y * 7 + p0.z * 2 + p0.w * 2 + p1.x * 1 + p1.y * 1 + p1.z * 1 + p1.w * 1 + q0.x * 1 + 8) >> 4)
                << 16;
    pixels.x |=
        ((p0.y * 5 + p0.z * 2 + p0.w * 2 + p1.x * 2 + p1.y * 1 + p1.z * 1 + p1.w * 1 + q0.x * 1 + q0.y * 1 + 8) >> 4)
        << 24;
    pixels.y = ((p0.y * 4 + p0.z * 1 + p0.w * 2 + p1.x * 2 + p1.y * 2 + p1.z * 1 + p1.w * 1 + q0.x * 1 + q0.y * 1 +
                 q0.z * 1 + 8) >>
                4)
               << 0;
    pixels.y |= ((p0.y * 3 + p0.z * 1 + p0.w * 1 + p1.x * 2 + p1.y * 2 + p1.z * 2 + p1.w * 1 + q0.x * 1 + q0.y * 1 +
                  q0.z * 1 + q0.w * 1 + 8) >>
                 4)
                << 8;
    pixels.y |= ((p0.y * 2 + p0.z * 1 + p0.w * 1 + p1.x * 1 + p1.y * 2 + p1.z * 2 + p1.w * 2 + q0.x * 1 + q0.y * 1 +
                  q0.z * 1 + q0.w * 1 + q1.x * 1 + 8) >>
                 4)
                << 16;
    pixels.y |= ((p0.y * 1 + p0.z * 1 + p0.w * 1 + p1.x * 1 + p1.y * 1 + p1.z * 2 + p1.w * 2 + q0.x * 2 + q0.y * 1 +
                  q0.z * 1 + q0.w * 1 + q1.x * 1 + q1.y * 1 + 8) >>
                 4)
                << 24;
    pixels.z = ((p0.z * 1 + p0.w * 1 + p1.x * 1 + p1.y * 1 + p1.z * 1 + p1.w * 2 + q0.x * 2 + q0.y * 2 + q0.z * 1 +
                 q0.w * 1 + q1.x * 1 + q1.y * 1 + q1.z * 1 + 8) >>
                4)
               << 0;
    pixels.z |= ((p0.w * 1 + p1.x * 1 + p1.y * 1 + p1.z * 1 + p1.w * 1 + q0.x * 2 + q0.y * 2 + q0.z * 2 + q0.w * 1 +
                  q1.x * 1 + q1.y * 1 + q1.z * 2 + 8) >>
                 4)
                << 8;
    pixels.z |= ((p1.x * 1 + p1.y * 1 + p1.z * 1 + p1.w * 1 + q0.x * 1 + q0.y * 2 + q0.z * 2 + q0.w * 2 + q1.x * 1 +
                  q1.y * 1 + q1.z * 3 + 8) >>
                 4)
                << 16;
    pixels.z |= ((p1.y * 1 + p1.z * 1 + p1.w * 1 + q0.x * 1 + q0.y * 1 + q0.z * 2 + q0.w * 2 + q1.x * 2 + q1.y * 1 +
                  q1.z * 4 + 8) >>
                 4)
                << 24;
    pixels.w |=
        ((p1.z * 1 + p1.w * 1 + q0.x * 1 + q0.y * 1 + q0.z * 1 + q0.w * 2 + q1.x * 2 + q1.y * 2 + q1.z * 5 + 8) >> 4)
        << 0;
    pixels.w |= ((p1.w * 1 + q0.x * 1 + q0.y * 1 + q0.z * 1 + q0.w * 1 + q1.x * 2 + q1.y * 2 + q1.z * 7 + 8) >> 4) << 8;
    buffer.Store4(offset - 8, pixels);
  } else if (type >= 2 && flat) {
    // 8-tap filter. Every output from its own sum, as in the 14-tap branch, and not from a running sum
    // v (v = p1.x * 3 + p1.y + p1.z + p1.w + q0.x, then v += -p1.x + q0.y and so on: the same values).
    // FXC compiled the running sum to
    //     iadd r2.x, r3.w, -r3.y          (q0.w - p1.x)
    //     ...
    //     iadd r3.yw, r3.wwww, -r4.xxxy   (q0.w - p1.y and q0.w - p1.z, over q0.w and p1.x)
    // and the console's driver ran the first of the two as r3.z - r3.x (p1.w - q0.x): the three samples
    // right of the edge came out wrong (docs/gpu-av1-decoder.md, "A paired read before an overlapping
    // in-place write"; testing/dxbc_scan/scan.py refuses that pair in the listings).
    pixels.y = p1.x;
    pixels.y |= ((p1.x * 3 + p1.y * 2 + p1.z + p1.w + q0.x + 4) >> 3) << 8;
    pixels.y |= ((p1.x * 2 + p1.y + p1.z * 2 + p1.w + q0.x + q0.y + 4) >> 3) << 16;
    pixels.y |= ((p1.x + p1.y + p1.z + p1.w * 2 + q0.x + q0.y + q0.z + 4) >> 3) << 24;

    pixels.z = (p1.y + p1.z + p1.w + q0.x * 2 + q0.y + q0.z + q0.w + 4) >> 3;
    pixels.z |= ((p1.z + p1.w + q0.x + q0.y * 2 + q0.z + q0.w * 2 + 4) >> 3) << 8;
    pixels.z |= ((p1.w + q0.x + q0.y + q0.z * 2 + q0.w * 3 + 4) >> 3) << 16;
    pixels.z |= q0.w << 24;
    buffer.Store2(offset - 4, pixels.yz);
  } else if (type == 1 && flat_uv) {
    pixels.y &= 0x0000ffff;
    pixels.z &= 0xffff0000;
    // 5-tap filter [1, 2, 2, 2, 1]
    pixels.y |= ((p1.y * 3 + p1.z * 2 + p1.w * 2 + q0.x + 4) >> 3) << 16;
    pixels.y |= ((p1.y + p1.z * 2 + p1.w * 2 + q0.x * 2 + q0.y + 4) >> 3) << 24;
    pixels.z |= ((p1.z + p1.w * 2 + q0.x * 2 + q0.y * 2 + q0.z + 4) >> 3);
    pixels.z |= ((p1.w + q0.x * 2 + q0.y * 2 + q0.z * 3 + 4) >> 3) << 8;
    buffer.Store2(offset - 4, pixels.yz);
  } else {
    uint hev = (abs(p1.w - p1.z) > limits.z || abs(q0.x - q0.y) > limits.z) ? 0xffffffff : 0;
    int ps1 = p1.z - 128;
    int ps0 = p1.w - 128;
    int qs0 = q0.x - 128;
    int qs1 = q0.y - 128;

    int f0 = clamp(ps1 - qs1, -128, 127) & hev;
    f0 = clamp(f0 + 3 * (qs0 - ps0), -128, 124);
    int f1 = min(f0 + 4, 127) >> 3;
    int f2 = (f0 + 3) >> 3;

    pixels.y &= 0x0000FFFF;
    pixels.z &= 0xFFFF0000;

    pixels.y |= (clamp(ps0 + f2, -128, 127) + 128) << 24;
    pixels.z |= clamp(qs0 - f1, -128, 127) + 128;

    f0 = ((f1 + 1) >> 1) & (~hev);

    pixels.y |= (clamp(ps1 + f0, -128, 127) + 128) << 16;
    pixels.z |= (clamp(qs1 - f0, -128, 127) + 128) << 8;
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
  const int addr = cb_planes[plane].y + block_x * 64 + (block_y * 4 + wi) * stride;

  if (!active) return;

  for (int col = 0; col < 16;) {
    uint edge = input_edges[local_offset + col];
    if (!edge) break;
    int level = edge & 63;
    int filter = (edge >> 6) & 3;
    int step = edge >> 8;

    if (level) {
      const int4 limits = cb_limits[level];
      filter_vertical_edge(dst_frame, addr + col * 4, limits, filter);
    }
    col += step;
  }
}
