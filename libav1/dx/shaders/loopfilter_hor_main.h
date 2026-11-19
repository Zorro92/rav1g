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

#include "loopfilter_common.h"

// A lane owns one complete frame word in each row: two columns of a 10-bit strip, or four
// columns of an 8-bit strip. The lane reads and writes its own samples throughout the pass.
#define LfWordsPerStrip (LfSampleBits / 8)
#define LfEdgesPerWord (16 / LfWordsPerStrip)
#define LfPixelGray (128 << LfLimitShift)
#define LfSignedMin (-LfPixelGray)
#define LfSignedMax (LfPixelGray - 1)
#define LfFlatThreshold (1 << LfLimitShift)

groupshared uint input_edges[64 * LfEdgesPerWord];
groupshared uint edge_masks[64];

struct LfSamples {
  int4 p0, p1, q0, q1;
};

uint filter_lf_column(inout LfSamples samples, int4 limits, uint type) {
  const int4 p0 = samples.p0;
  const int4 p1 = samples.p1;
  const int4 q0 = samples.q0;
  const int4 q1 = samples.q1;

  int mask = abs(p1.z - p1.w) <= limits.x && abs(q0.x - q0.y) <= limits.x &&
             abs(p1.w - q0.x) * 2 + (abs(p1.z - q0.y) >> 1) <= limits.y;

  mask &= (abs(p1.y - p1.z) <= limits.x && abs(q0.y - q0.z) <= limits.x) | (type == 0);
  mask &= (abs(p1.x - p1.y) <= limits.x && abs(q0.z - q0.w) <= limits.x) | (type <= 1);

  if (!mask) return 0;

  const int t = LfFlatThreshold;
  const bool flat_uv = abs(p1.y - p1.w) <= t && abs(p1.z - p1.w) <= t &&
                       abs(q0.y - q0.x) <= t && abs(q0.z - q0.x) <= t;
  const bool flat = flat_uv && abs(p1.x - p1.w) <= t && abs(q0.w - q0.x) <= t;
  const bool flat2 = abs(p0.y - p1.w) <= t && abs(p0.z - p1.w) <= t && abs(p0.w - p1.w) <= t &&
                     abs(q1.x - q0.x) <= t && abs(q1.y - q0.x) <= t && abs(q1.z - q0.x) <= t;

  if (type == 3 && flat && flat2) {
    samples.p0.z = (p0.y * 7 + p0.z * 2 + p0.w * 2 + p1.x + p1.y + p1.z + p1.w + q0.x + 8) >> 4;
    samples.p0.w = (p0.y * 5 + p0.z * 2 + p0.w * 2 + p1.x * 2 + p1.y + p1.z + p1.w + q0.x + q0.y + 8) >> 4;
    samples.p1.x = (p0.y * 4 + p0.z + p0.w * 2 + p1.x * 2 + p1.y * 2 + p1.z + p1.w + q0.x + q0.y + q0.z + 8) >> 4;
    samples.p1.y = (p0.y * 3 + p0.z + p0.w + p1.x * 2 + p1.y * 2 + p1.z * 2 + p1.w +
                    q0.x + q0.y + q0.z + q0.w + 8) >> 4;
    samples.p1.z = (p0.y * 2 + p0.z + p0.w + p1.x + p1.y * 2 + p1.z * 2 + p1.w * 2 +
                    q0.x + q0.y + q0.z + q0.w + q1.x + 8) >> 4;
    samples.p1.w = (p0.y + p0.z + p0.w + p1.x + p1.y + p1.z * 2 + p1.w * 2 +
                    q0.x * 2 + q0.y + q0.z + q0.w + q1.x + q1.y + 8) >> 4;
#if LfSampleBits == 8
    // Keep the paired-read workaround in the 8-bit legacy shader's 14-tap sum.
    samples.q0.x = (p1.x + p1.y + p1.z + p1.w * 2 + q0.x * 2 + q0.y * 2 + q0.z + q0.w +
                    q1.x + q1.y + q1.z + p0.z + p0.w + 8) >> 4;
#else
    samples.q0.x = (p0.z + p0.w + p1.x + p1.y + p1.z + p1.w * 2 + q0.x * 2 + q0.y * 2 +
                    q0.z + q0.w + q1.x + q1.y + q1.z + 8) >> 4;
#endif
    samples.q0.y = (p0.w + p1.x + p1.y + p1.z + p1.w + q0.x * 2 + q0.y * 2 +
                    q0.z * 2 + q0.w + q1.x + q1.y + q1.z * 2 + 8) >> 4;
    samples.q0.z = (p1.x + p1.y + p1.z + p1.w + q0.x + q0.y * 2 + q0.z * 2 +
                    q0.w * 2 + q1.x + q1.y + q1.z * 3 + 8) >> 4;
    samples.q0.w = (p1.y + p1.z + p1.w + q0.x + q0.y + q0.z * 2 + q0.w * 2 + q1.x * 2 + q1.y + q1.z * 4 + 8) >> 4;
    samples.q1.x = (p1.z + p1.w + q0.x + q0.y + q0.z + q0.w * 2 + q1.x * 2 + q1.y * 2 + q1.z * 5 + 8) >> 4;
    samples.q1.y = (p1.w + q0.x + q0.y + q0.z + q0.w + q1.x * 2 + q1.y * 2 + q1.z * 7 + 8) >> 4;
    return 6;
  }
  if (type >= 2 && flat) {
    int v = p1.x * 3 + p1.y + p1.z + p1.w + q0.x;
    samples.p1.y = (v + p1.y + 4) >> 3;
    v += -p1.x + q0.y;
    samples.p1.z = (v + p1.z + 4) >> 3;
    v += -p1.x + q0.z;
    samples.p1.w = (v + p1.w + 4) >> 3;
    v += -p1.x + q0.w;
    samples.q0.x = (v + q0.x + 4) >> 3;
    v += -p1.y + q0.w;
    samples.q0.y = (v + q0.y + 4) >> 3;
    v += -p1.z + q0.w;
    samples.q0.z = (v + q0.z + 4) >> 3;
    return 3;
  }
  if (type == 1 && flat_uv) {
    // 5-tap filter [1, 2, 2, 2, 1]
    samples.p1.z = (p1.y * 3 + p1.z * 2 + p1.w * 2 + q0.x + 4) >> 3;
    samples.p1.w = (p1.y + p1.z * 2 + p1.w * 2 + q0.x * 2 + q0.y + 4) >> 3;
    samples.q0.x = (p1.z + p1.w * 2 + q0.x * 2 + q0.y * 2 + q0.z + 4) >> 3;
    samples.q0.y = (p1.w + q0.x * 2 + q0.y * 2 + q0.z * 3 + 4) >> 3;
    return 2;
  }

  uint hev = (abs(p1.w - p1.z) > limits.z || abs(q0.x - q0.y) > limits.z) ? 0xffffffff : 0;
  int ps1 = p1.z - LfPixelGray;
  int ps0 = p1.w - LfPixelGray;
  int qs0 = q0.x - LfPixelGray;
  int qs1 = q0.y - LfPixelGray;

  int f0 = clamp(ps1 - qs1, LfSignedMin, LfSignedMax) & hev;
#if LfSampleBits == 8
  f0 = clamp(f0 + 3 * (qs0 - ps0), -128, 124);
  int f1 = min(f0 + 4, 127) >> 3;
  int f2 = (f0 + 3) >> 3;
#else
  f0 = clamp(f0 + 3 * (qs0 - ps0), LfSignedMin, LfSignedMax);
  int f1 = clamp(f0 + 4, LfSignedMin, LfSignedMax) >> 3;
  int f2 = clamp(f0 + 3, LfSignedMin, LfSignedMax) >> 3;
#endif

  samples.p1.w = clamp(ps0 + f2, LfSignedMin, LfSignedMax) + LfPixelGray;
  samples.q0.x = (clamp(qs0 - f1, LfSignedMin, LfSignedMax) + LfPixelGray);

  f0 = ((f1 + 1) >> 1) & (~hev);

  samples.p1.z = clamp(ps1 + f0, LfSignedMin, LfSignedMax) + LfPixelGray;
  samples.q0.y = clamp(qs1 - f0, LfSignedMin, LfSignedMax) + LfPixelGray;
  return 2;
}

int4 unpack_lf_column(uint4 words, uint shift) {
  const uint mask = (1u << LfSampleBits) - 1;
  return int4((words.x >> shift) & mask, (words.y >> shift) & mask, (words.z >> shift) & mask,
              (words.w >> shift) & mask);
}

void pack_lf_column(inout uint4 words, int4 column, uint shift) {
  words.x |= uint(column.x) << shift;
  words.y |= uint(column.y) << shift;
  words.z |= uint(column.z) << shift;
  words.w |= uint(column.w) << shift;
}

void filter_horizontal_word(uint offset, uint stride, int4 limits, uint type) {
  uint4 p0 = uint4(0, 0, 0, 0);
  uint4 p1 = uint4(0, 0, dst_frame.Load(offset - 2 * stride), dst_frame.Load(offset - stride));
  uint4 q0 = uint4(dst_frame.Load(offset), dst_frame.Load(offset + stride), 0, 0);
  uint4 q1 = uint4(0, 0, 0, 0);
  // Read just the taps that can affect this filter. In particular the 4-tap edge at a
  // strip boundary must not read the previous strip's 4-tap edge four rows above it.
  if (type >= 1) {
    p1.y = dst_frame.Load(offset - 3 * stride);
    q0.z = dst_frame.Load(offset + 2 * stride);
  }
  if (type >= 2) {
    p1.x = dst_frame.Load(offset - 4 * stride);
    q0.w = dst_frame.Load(offset + 3 * stride);
  }
  if (type == 3) {
    p0.y = dst_frame.Load(offset - 7 * stride);
    p0.z = dst_frame.Load(offset - 6 * stride);
    p0.w = dst_frame.Load(offset - 5 * stride);
    q1.x = dst_frame.Load(offset + 4 * stride);
    q1.y = dst_frame.Load(offset + 5 * stride);
    q1.z = dst_frame.Load(offset + 6 * stride);
  }

  uint4 out_p0 = uint4(0, 0, 0, 0);
  uint4 out_p1 = uint4(0, 0, 0, 0);
  uint4 out_q0 = uint4(0, 0, 0, 0);
  uint4 out_q1 = uint4(0, 0, 0, 0);
  uint radius = 0;
  [unroll] for (uint shift = 0; shift < 32; shift += LfSampleBits) {
    LfSamples samples = {unpack_lf_column(p0, shift), unpack_lf_column(p1, shift),
                         unpack_lf_column(q0, shift), unpack_lf_column(q1, shift)};
    const uint column_radius = filter_lf_column(samples, limits, type);
    radius = max(radius, column_radius);
    pack_lf_column(out_p0, samples.p0, shift);
    pack_lf_column(out_p1, samples.p1, shift);
    pack_lf_column(out_q0, samples.q0, shift);
    pack_lf_column(out_q1, samples.q1, shift);
  }

  if (radius == 0) return;
  dst_frame.Store(offset - 2 * stride, out_p1.z);
  dst_frame.Store(offset - stride, out_p1.w);
  dst_frame.Store(offset, out_q0.x);
  dst_frame.Store(offset + stride, out_q0.y);
  if (radius >= 3) {
    dst_frame.Store(offset - 3 * stride, out_p1.y);
    dst_frame.Store(offset + 2 * stride, out_q0.z);
  }
  if (radius == 6) {
    dst_frame.Store(offset - 6 * stride, out_p0.z);
    dst_frame.Store(offset - 5 * stride, out_p0.w);
    dst_frame.Store(offset - 4 * stride, out_p1.x);
    dst_frame.Store(offset + 3 * stride, out_q0.w);
    dst_frame.Store(offset + 4 * stride, out_q1.x);
    dst_frame.Store(offset + 5 * stride, out_q1.y);
  }
}

[numthreads(64, 1, 1)]
void main(uint3 thread : SV_DispatchThreadID) {
  const uint lane = thread.x & 63;
  const uint word = lane % LfWordsPerStrip;
  const uint block_id = thread.x / LfWordsPerStrip + cb_block_id_offset;
  const bool active = thread.x < cb_wicount;

  if (active) {
    const uint list = (cb_offset_base + block_id) * 32 + word * LfEdgesPerWord * 2;
    uint mask = 0;
    [unroll] for (uint base = 0; base < LfEdgesPerWord; base += 8) {
      uint4 pairs = lf_blocks.Load4(list + base * 2);
      [unroll] for (uint pair = 0; pair < 4; ++pair) {
        uint index = base + pair * 2;
        uint low = prepare_lf_edge(pairs[pair]);
        uint high = prepare_lf_edge(pairs[pair] >> 16);
        input_edges[lane * LfEdgesPerWord + index] = low;
        input_edges[lane * LfEdgesPerWord + index + 1] = high;
        mask |= ((low != 0) << index) | ((high != 0) << (index + 1));
      }
    }
    edge_masks[lane] = mask << (word * LfEdgesPerWord);
  }
  GroupMemoryBarrierWithGroupSync();
  if (!active) return;

  const uint first_lane = lane - word;
  uint remaining = 0;
  [unroll] for (uint i = 0; i < LfWordsPerStrip; ++i) {
    remaining |= edge_masks[first_lane + i];
  }
  const uint block_x = block_id % cb_block_cols;
  const uint block_y = block_id / cb_block_cols;
  const uint stride = cb_planes[cb_plane].x;
  const uint addr = cb_planes[cb_plane].y + (block_x * LfWordsPerStrip + word) * 4 + block_y * 64 * stride;

  // Across adjacent transform edges at distance 4, 8, or at least 16, the smaller transform
  // bounds the read radius on both edges to 2, 4, or 7 respectively (chroma can be shorter).
  // Within a strip keep the existing top-to-bottom order, with no cross-lane pixel handoff.
  while (remaining != 0) {
    uint row = firstbitlow(remaining);
    uint edge = input_edges[first_lane * LfEdgesPerWord + row];
    filter_horizontal_word(addr + row * 4 * stride, stride, lf_edge_limits(edge), edge & 3);
    remaining &= remaining - 1;
  }
}
