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
// Each column publishes three words: the right pair of the top row, and both
// pairs of the bottom row. Each word has two banks that alternate by diagonal.
#define UNIT_COLUMNS 8
groupshared uint unit_edges[3][2][UNIT_COLUMNS];
groupshared uint left_edges[16];

uint2 load_above(int base, int stride, int col, int above_available, int left_available) {
  uint2 pixels = uint2(0, 0x01ff0000);
  if (above_available)
    pixels = dst_frame.Load2(base - stride + min(above_available - 1, col) * 8);
  else if (left_available)
    pixels.y = dst_frame.Load(base - 4);
  if (col >= above_available) {
    const uint last = (pixels.y >> 16) & 1023;
    const uint pair = last | (last << 16);
    return uint2(pair, pair);
  }
  return uint2(pixels.x & 0x03ff03ff, pixels.y & 0x03ff03ff);
}

uint4 pack_taps(int4 lo, int4 hi) {
  // Adding the signed low tap keeps its borrow in the high half of the word.
  return uint4((uint)lo.x + ((uint)hi.x << 16), (uint)lo.y + ((uint)hi.y << 16),
               (uint)lo.z + ((uint)hi.z << 16), (uint)lo.w + ((uint)hi.w << 16));
}

// One lane owns two adjacent samples of a 4x2 unit, and therefore one
// frame word. A lane visits successive unit rows in the same column.
[numthreads(4, 8, 1)] void main(uint3 thread : SV_GroupThreadID, uint3 gid : SV_GroupID) {
  if (gid.x * 8 >= cb_wi_count) return;

  const uint3 block = pred_blocks.Load3((cb_pass_offset + gid.x) << 4);
  const int columns = 1 << (block.y & 3);
  const int rows = 2 << (block.z & 3);
  const int plane = (block.y >> 3) & 3;
  const int mode = (block.z >> 4) & 7;
  // The plane's constants by a select, not by a load that waits for the record (filter intra is luma).
  const int4 plane_cb = plane == 0 ? cb_planes[0] : plane == 1 ? cb_planes[1] : cb_planes[2];
  const int stride = plane_cb.x;
  const int x = 8 * (block.x & 0xffff);
  const int y = 4 * (block.x >> 16);
  const int base = plane_cb.y + x + y * stride;
  const int above_available = (block.y >> 10) & 63;
  const int left_available = ((block.y >> 16) & 63) << 2;

  const int pair = thread.x;
  const int col = thread.y;
  const int lane = pair + 4 * col;

  // A column starts on the diagonal with the same index, so seed its bank
  // for that diagonal with the original above edge. Use the unused
  // top-right word of column zero in bank zero for the block corner.
  if (lane < columns) {
    const uint2 pixels = load_above(base, stride, lane, above_available, left_available);
    unit_edges[1][lane & 1][lane] = pixels.x;
    unit_edges[2][lane & 1][lane] = pixels.y;
  }
  if (lane < rows) {
    uint left0, left1;
    if (left_available) {
      left0 = dst_frame.Load(base - 4 + min(2 * lane, left_available - 1) * stride) >> 16;
      left1 = dst_frame.Load(base - 4 + min(2 * lane + 1, left_available - 1) * stride) >> 16;
    } else {
      left0 = 513;
      if (above_available) left0 = dst_frame.Load(base - stride) & 1023;
      left1 = left0;
    }
    left_edges[lane] = left0 | (left1 << 16);
    if (lane == 0) {
      uint corner = left0;
      if (above_available && left_available) corner = dst_frame.Load(base - stride - 4) >> 16;
      if (!above_available && !left_available) corner = 512;
      unit_edges[0][0][0] = corner;
    }
  }

  const int tap = pair * 2;
  const uint4 a = pack_taps(cb_filter[mode][tap][0], cb_filter[mode][tap + 1][0]);
  const uint4 b = pack_taps(cb_filter[mode][tap][1], cb_filter[mode][tap + 1][1]);
  const bool add_residual = (block.y & (1 << 5)) != 0;
  const int res_stride = plane_cb.z;
  const int word_x = col * 8 + (pair & 1) * 4;
  const int word_y = pair >> 1;
  const int frame_offset = base + word_x + word_y * stride;
  const int res_offset = plane_cb.w + x + word_x + (y + word_y) * res_stride;

  // Keep one residual read ahead of the wavefront. The residual belongs
  // only to the frame store, while the next diagonal uses the prediction.
  // All residual words of the lane's column, loaded here at once: a lane visits one unit row per
  // diagonal, and a load issued one diagonal ahead waits out most of its latency in every diagonal of
  // a tall block. The queue is shifted by one per row, so that it stays in registers.
  int queue[16];
  [unroll] for (int q = 0; q < 16; ++q) {
    queue[q] = 0;
    if (add_residual && col < columns && q < rows) queue[q] = (int)residuals.Load(res_offset + 2 * q * res_stride);
  }

  GroupMemoryBarrierWithGroupSync();

  uint corner = 0;
  if (col == 0) corner = unit_edges[0][0][0];
  else if (col < columns) corner = (uint)unit_edges[2][(col - 1) & 1][col - 1] >> 16;

  for (int diagonal = 0; diagonal < columns + rows - 1; ++diagonal) {
    if (diagonal != 0) GroupMemoryBarrierWithGroupSync();
    const int row = diagonal - col;
    if (col >= columns || row < 0 || row >= rows) continue;
    const int previous = diagonal & 1;

    const int residual = queue[0];
    [unroll] for (int q = 0; q < 15; ++q) queue[q] = queue[q + 1];

    const uint above01 = unit_edges[1][previous][col];
    const uint above23 = unit_edges[2][previous][col];
    uint left0, left1;
    if (col == 0) {
      const uint left = left_edges[row];
      left0 = left & 1023;
      left1 = left >> 16;
    } else {
      left0 = (uint)unit_edges[0][previous][col - 1] >> 16;
      left1 = (uint)unit_edges[2][previous][col - 1] >> 16;
    }

    // Accumulate both signed dot products modulo 2^32. With 10-bit
    // samples and the AV1 taps, each rounded sum is in [-12268, 28652],
    // so the low half needs only a 0x8000 correction to undo its borrow
    // into the high half. The signed shifts include the AV1 divide by 16.
    const uint top_sum = (above01 & 1023) * a.y + (above01 >> 16) * a.z +
                         (above23 & 1023) * a.w + (above23 >> 16) * b.x;
    const uint left_sum = corner * a.x + left0 * b.y + left1 * b.z;
    const uint sum = top_sum + left_sum + 0x00080008u;
    int lo = clamp((int)(sum << 16) >> 20, 0, 1023);
    int hi = clamp((int)(sum + 0x8000u) >> 20, 0, 1023);

    if (pair != 0) unit_edges[pair - 1][previous ^ 1][col] = lo | (hi << 16);
    // This row's bottom-left input is the next row's top-left input.
    corner = left1;

    if (add_residual) {
      lo = clamp(lo + ((int)((uint)residual << 16) >> 16), 0, 1023);
      hi = clamp(hi + (residual >> 16), 0, 1023);
    }
    dst_frame.Store(frame_offset + 2 * row * stride, lo | (hi << 16));
  }
}
