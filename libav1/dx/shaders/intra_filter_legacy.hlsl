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

#define ROWS 8
groupshared int mem[ROWS][8];

groupshared int4 loc_above[8];
groupshared int loc_left[32];
groupshared int loc_corner[18];

// One thread group predicts one block: 8 threads in x, one per sample of a 4x2 unit, and 8 in y, one per
// row of units. The units are predicted in a wavefront: a unit needs the unit to its left and the unit
// above it. The thread row y takes column c in step c + y + 1 of a pass, one step after the row above took it. Every
// step has a group sync after the samples of its units are computed and another after they are handed on,
// so the steps follow each other on any GPU. (Upstream had plain group memory barriers in thread dependent
// loops here, which order nothing: the wavefront held only where the 64 threads of the group ran in
// lockstep.)
[numthreads(8, ROWS, 1)] void main(uint3 thread
                                   : SV_DispatchThreadID, uint3 gid
                                   : SV_GroupID) {
  // The block is the group's, so it and the loop bounds made from it are the same for all threads of the
  // group: the group syncs below are not in thread dependent flow control.
  if (gid.x * 8 >= cb_wi_count) return;

  uint3 block = pred_blocks.Load3((cb_pass_offset + gid.x) << 4);

  const int wi = thread.x & 7;
  int x = 4 * (block.x & 0xffff);
  int y = 4 * (block.x >> 16);

  //    block.y bits:
  //    0    3        bw_log
  //    3    2        plane
  //    5    1        non skip
  //    6    4        mode
  //        all mods except intra_bc:
  //    10    6        above_available
  //    16    6        left_available
  //            dir mode params:
  //    22    2        upsample
  //    24    2        edge_filter above
  //    26    2        edge_filter left
  //    28    3        mode_angle
  //    31    1        inter_intra?
  //            CFL:
  //    22    4        alpha
  //
  //    block.z
  //        intra_bc - mv
  //        inter-intra    - coef. table indexes;
  //        filter - mode_info0 = txh | (filter_mode << 4);
  //    block.w - reserved; (prob. used for sorting);

  const int bw = 1 << (block.y & 3);
  const int bh = 2 << (block.z & 3);
  const int plane = (block.y >> 3) & 3;
  const int mode = (block.z >> 4) & 7;
  const int stride = cb_planes[plane].x;
  const int offset = cb_planes[plane].y + x + y * stride;

  const int above_available = (block.y >> 10) & 63;
  const int left_available = ((block.y >> 16) & 63) << 2;
  if (wi < bw) {
    uint pixels = above_available ? dst_frame.Load(offset - stride + min(above_available - 1, wi) * 4)
                                  : left_available ? dst_frame.Load(offset - 4) : 0x7f7f7f7f;
    if (wi >= above_available) pixels = (pixels >> 24) * 0x01010101;
    loc_above[wi].x = (pixels >> 0) & 255;
    loc_above[wi].y = (pixels >> 8) & 255;
    loc_above[wi].z = (pixels >> 16) & 255;
    loc_above[wi].w = (pixels >> 24) & 255;
  }
  GroupMemoryBarrierWithGroupSync();

  int row;
  for (row = wi; row < bh * 2; row += 8) {
    const int l_addr = offset - 4 + min(row, left_available - 1) * stride;
    int left = left_available ? (dst_frame.Load(l_addr) >> 24) : above_available ? loc_above[0].x : 129;

    loc_left[row] = left;
    if (row & 1) loc_corner[(row + 1) >> 1] = left;
  }

  if (wi == 0) {
    loc_corner[0] = (left_available && above_available) ? (dst_frame.Load(offset - 4 - stride) >> 24)
                                                        : (left_available || above_available) ? loc_above[0].x : 128;
  }

  GroupMemoryBarrierWithGroupSync();

  int4 filter0 = cb_filter[mode][wi][0];
  int3 filter1 = cb_filter[mode][wi][1].xyz;

  for (int row0 = 0; row0 < bh; row0 += ROWS) {
    row = row0 + thread.y;
    for (int wave = 0; wave < bw + ROWS; ++wave) {
      const int col = wave - (int)thread.y - 1;
      const bool unit = row < bh && col >= 0 && col < bw;
      int4 p14 = int4(0, 0, 0, 0);
      if (unit) {
        int p0 = loc_corner[row];
        p14 = loc_above[col];
        int p5 = loc_left[2 * row];
        int p6 = loc_left[2 * row + 1];

        int pixel = p0 * filter0.x + p14.x * filter0.y + p14.y * filter0.z + p14.z * filter0.w + p14.w * filter1.x +
                    p5 * filter1.y + p6 * filter1.z;
        pixel = clamp((pixel + 8) >> 4, 0, 255);
        mem[thread.y][wi] = pixel;
      }

      GroupMemoryBarrierWithGroupSync();

      if (unit && wi < 2) {
        int4 pix;
        pix.x = mem[thread.y][wi * 4 + 0];
        pix.y = mem[thread.y][wi * 4 + 1];
        pix.z = mem[thread.y][wi * 4 + 2];
        pix.w = mem[thread.y][wi * 4 + 3];

        loc_left[2 * row + wi] = pix.w;
        if (wi == 1) {
          loc_above[col] = pix;
          loc_corner[row] = p14.w;
        }
        uint pixel4;
        pixel4 = pix.x;
        pixel4 |= pix.y << 8;
        pixel4 |= pix.z << 16;
        pixel4 |= pix.w << 24;

        const int addr = offset + col * 4 + (row * 2 + wi) * stride;
        dst_frame.Store(addr, pixel4);
      }

      GroupMemoryBarrierWithGroupSync();
    }
  }

  if (block.y & (1 << 5)) {
    const int res_stride = cb_planes[plane].z;
    const int res_offset = cb_planes[plane].w + 2 * x + y * res_stride;
    // Every word of the block once, spread over the 64 threads of the group. (Upstream spread the words
    // over the 8 threads of a row and let all 8 rows do the same load, add and store, which is right only
    // where the rows run in lockstep: every row has to load before any row stores.)
    for (int idx = wi + thread.y * 8; idx < 2 * bw * bh; idx += 8 * ROWS) {
      // The column of the word: idx modulo the block's width in words. The mask comes from a table on
      // purpose: FXC makes a bit field instruction of width bw_log2 out of idx & (bw - 1) and the
      // multiplications below, and a bit field of width 0 (a block one word wide) is one of the things GPUs
      // do not agree on.
      static const int word_mask[4] = {0, 1, 3, 7};
      const int wx = idx & word_mask[block.y & 3];
      const int wy = idx >> (block.y & 3);  //(block.y & 3) = bw_log2
      const int addr = offset + 4 * wx + stride * wy;
      uint pix = dst_frame.Load(addr);

      const int res_addr = res_offset + 8 * wx + res_stride * wy;
      int2 r = residuals.Load2(res_addr);
      uint result = (clamp((int)((pix >> 0) & 255) + (int)((r.x << 16) >> 16), 0, 255) << 0) |
                    (clamp((int)((pix >> 8) & 255) + (int)(r.x >> 16), 0, 255) << 8) |
                    (clamp((int)((pix >> 16) & 255) + (int)((r.y << 16) >> 16), 0, 255) << 16) |
                    (clamp((int)((pix >> 24) & 255) + (int)(r.y >> 16), 0, 255) << 24);
      dst_frame.Store(addr, result);
    }
  }
}
