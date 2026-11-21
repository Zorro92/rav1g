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

cbuffer IntraDataCommon : register(b0) {
  int4 cb_planes[3];
  int4 cb_flags;
  int4 cb_filter[5][8][2];
  int4 cb_mode_params_lut[16][7];
  int4 cb_sm_weight_arrays[128];
};

cbuffer PSSLIntraSRT : register(b1) {
  uint cb_wi_count;
  int cb_pass_offset;
  int4 cb_counts0;
  int4 cb_counts1;
};

ByteAddressBuffer pred_blocks : register(t0);
ByteAddressBuffer residuals : register(t1);
ByteAddressBuffer wedge_mask : register(t2);
RWByteAddressBuffer dst_frame : register(u0);

// A 32x32 transform has up to eight columns and sixteen rows of 4x2 units.
// Each entry carries two predicted pixels in the low half and two
// residual-adjusted pixels in the high half, with a bank for each diagonal parity.
#define UNIT_COLUMNS 8
groupshared uint unit_pairs[4][2][UNIT_COLUMNS];
groupshared uint left_edges[16];

uint load_above(int base, int stride, int col, int above_available, int left_available) {
  uint pixels = 0x7f7f7f7f;
  if (above_available)
    pixels = dst_frame.Load(base - stride + min(above_available - 1, col) * 4);
  else if (left_available)
    pixels = dst_frame.Load(base - 4);
  if (col >= above_available) pixels = (pixels >> 24) * 0x01010101u;
  return pixels;
}

uint4 pack_taps(int4 lo, int4 hi) {
  // Keep the sign of the low tap in the packed sum by adding it as a full word.
  return uint4((uint)lo.x + ((uint)hi.x << 16), (uint)lo.y + ((uint)hi.y << 16),
               (uint)lo.z + ((uint)hi.z << 16), (uint)lo.w + ((uint)hi.w << 16));
}

uint predict_pair(uint above01, uint above23, uint corner, uint left0, uint left1, uint4 a, uint4 b) {
  // Two signed dot products in one modulo-2^32 sum. In every AV1 tap row,
  // the negative taps sum to at least -12 and the positive taps to at most
  // 28. With 8-bit inputs and rounding (+8), each sum is in [-3052, 7148].
  // Adding 0x8000 before extracting the high half cancels the low half's
  // borrow. The signed shifts also perform the AV1 divide by 16.
  const uint top_sum = (above01 & 255) * a.y + ((above01 >> 8) & 255) * a.z +
                       (above23 & 255) * a.w + ((above23 >> 8) & 255) * b.x;
  const uint left_sum = corner * a.x + left0 * b.y + left1 * b.z;
  const uint sum = top_sum + left_sum + 0x00080008u;
  const uint lo = clamp((int)(sum << 16) >> 20, 0, 255);
  const uint hi = clamp((int)(sum + 0x8000u) >> 20, 0, 255);
  return lo | (hi << 8);
}

uint add_residual_pair(uint pair, int residual) {
  const uint lo = clamp((int)(pair & 255) + ((int)((uint)residual << 16) >> 16), 0, 255);
  const uint hi = clamp((int)(pair >> 8) + (residual >> 16), 0, 255);
  return lo | (hi << 8);
}

// Four lanes per column, each predicting one adjacent pair of pixels.
// The right pair's lane owns the entire four-pixel frame word for its row.
[numthreads(4, 8, 1)] void main(uint3 thread : SV_GroupThreadID, uint3 gid : SV_GroupID) {
  if (gid.x * 8 >= cb_wi_count) return;

  const uint3 block = pred_blocks.Load3((cb_pass_offset + gid.x) << 4);
  const int columns = 1 << (block.y & 3);
  const int rows = 2 << (block.z & 3);
  const int plane = (block.y >> 3) & 3;
  const int mode = (block.z >> 4) & 7;
#ifdef INTRA_FILTER_PREFETCH
  // The plane's constants by a select, not by a load that waits for the record (filter intra is luma).
  const int4 plane_cb = plane == 0 ? cb_planes[0] : plane == 1 ? cb_planes[1] : cb_planes[2];
#else
#define plane_cb cb_planes[plane]
#endif
  const int stride = plane_cb.x;
  const int x = 4 * (block.x & 0xffff);
  const int y = 4 * (block.x >> 16);
  const int base = plane_cb.y + x + y * stride;
  const int above_available = (block.y >> 10) & 63;
  const int left_available = ((block.y >> 16) & 63) << 2;

  const int pair = thread.x;
  const int col = thread.y;
  const int lane = pair + 4 * col;

  // Column c starts on diagonal c. Seed the corresponding bank with its
  // above edge and use the unused upper word of column zero for the corner.
  if (lane < columns) {
    const uint pixels = load_above(base, stride, lane, above_available, left_available);
    unit_pairs[2][lane & 1][lane] = pixels & 65535;
    unit_pairs[3][lane & 1][lane] = pixels >> 16;
  }
  if (lane < rows) {
    uint left0, left1;
    if (left_available) {
      left0 = dst_frame.Load(base - 4 + min(2 * lane, left_available - 1) * stride) >> 24;
      left1 = dst_frame.Load(base - 4 + min(2 * lane + 1, left_available - 1) * stride) >> 24;
    } else {
      left0 = 129;
      if (above_available) left0 = dst_frame.Load(base - stride) & 255;
      left1 = left0;
    }
    left_edges[lane] = left0 | (left1 << 8);
    if (lane == 0) {
      uint corner = left0;
      if (above_available && left_available) corner = dst_frame.Load(base - stride - 4) >> 24;
      if (!above_available && !left_available) corner = 128;
      unit_pairs[1][0][0] = corner;
    }
  }

  const int tap = pair * 2;
  const uint4 a = pack_taps(cb_filter[mode][tap][0], cb_filter[mode][tap + 1][0]);
  const uint4 b = pack_taps(cb_filter[mode][tap][1], cb_filter[mode][tap + 1][1]);
  const bool add_residual = (block.y & (1 << 5)) != 0;
  const int res_stride = plane_cb.z;
  const int word_row = pair >> 1;
  const int frame_offset = base + 4 * col + word_row * stride;
  const int res_offset = plane_cb.w + 2 * x + 8 * col + (pair & 1) * 4 + (y + word_row) * res_stride;

#ifdef INTRA_FILTER_PREFETCH
  // All residual words of the lane's column, loaded here at once: a lane visits one unit row per
  // diagonal, and a load issued one diagonal ahead waits out most of its latency in every diagonal of
  // a tall block. The queue is shifted by one per row, so that it stays in registers.
  int queue[16];
  [unroll] for (int q = 0; q < 16; ++q) {
    queue[q] = 0;
    if (add_residual && col < columns && q < rows) queue[q] = (int)residuals.Load(res_offset + 2 * q * res_stride);
  }
#else
  int next_residual = 0;
  if (add_residual && col < columns) next_residual = (int)residuals.Load(res_offset);
#endif

  GroupMemoryBarrierWithGroupSync();

  uint corner = 0;
  if (col == 0) corner = unit_pairs[1][0][0];
  else if (col < columns) corner = (uint)unit_pairs[3][(col - 1) & 1][col - 1] >> 8;

  for (int diagonal = 0; diagonal < columns + rows - 1; ++diagonal) {
    const int row = diagonal - col;
    const bool active = col < columns && row >= 0 && row < rows;
    const int previous = diagonal & 1;
    const int current = previous ^ 1;
    uint output = 0;
    if (active) {
#ifdef INTRA_FILTER_PREFETCH
      const int residual = queue[0];
      [unroll] for (int q = 0; q < 15; ++q) queue[q] = queue[q + 1];
#else
      const int residual = next_residual;
      if (add_residual && row + 1 < rows)
        next_residual = (int)residuals.Load(res_offset + 2 * (row + 1) * res_stride);
#endif

      const uint above01 = unit_pairs[2][previous][col];
      const uint above23 = unit_pairs[3][previous][col];
      uint left0, left1;
      if (col == 0) {
        const uint left = left_edges[row];
        left0 = left & 255;
        left1 = left >> 8;
      } else {
        left0 = ((uint)unit_pairs[1][previous][col - 1] >> 8) & 255;
        left1 = ((uint)unit_pairs[3][previous][col - 1] >> 8) & 255;
      }
      const uint predicted = predict_pair(above01, above23, corner, left0, left1, a, b);
      corner = left1;
      output = add_residual ? add_residual_pair(predicted, residual) : predicted;
      unit_pairs[pair][current][col] = predicted | (output << 16);
    }

    // The same sync makes the prediction visible to the next diagonal and the
    // corrected pair visible to the word owner. The next diagonal writes the other bank.
    GroupMemoryBarrierWithGroupSync();
    if (active && (pair & 1)) {
      const uint first = (uint)unit_pairs[pair - 1][current][col] >> 16;
      dst_frame.Store(frame_offset + 2 * row * stride, first | (output << 16));
    }
  }
}
