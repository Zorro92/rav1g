/*
 * SPDX-License-Identifier: BSD-2-Clause
 *
 * This source code is subject to the terms of the BSD 2 Clause License and the Alliance for Open Media
 * Patent License 1.0. If the BSD 2 Clause License was not distributed with this source code in the LICENSE
 * file, you can obtain it at www.aomedia.org/license/software. If the Alliance for Open Media Patent
 * License 1.0 was not distributed with this source code in the PATENTS file, you can obtain it at
 * www.aomedia.org/license/patent.
 */

RWByteAddressBuffer pool : register(u0);

#define SP_TEXTURE
#define SP_LUMA_REGISTER u1
#define SP_CHROMA_REGISTER u2
#include "output_sp.h"

cbuffer cb_copy_sp_tex : register(b0) {
  uint cb_width;
  uint cb_height;
  uint cb_luma_group_rows;
  uint cb_hbd;
  uint cb_y_offset;
  uint cb_u_offset;
  uint cb_v_offset;
  uint cb_y_stride;
  uint cb_uv_stride;
  uint cb_neutral_chroma;
};

void load8(uint row, uint x, out int4 a, out int4 b) {
  if (cb_hbd) {
    const uint4 w = pool.Load4(row + x * 2);
    a = int4(w.x & 0xffff, w.x >> 16, w.y & 0xffff, w.y >> 16);
    b = int4(w.z & 0xffff, w.z >> 16, w.w & 0xffff, w.w >> 16);
  } else {
    const uint2 w = pool.Load2(row + x);
    a = int4(w.x & 255, (w.x >> 8) & 255, (w.x >> 16) & 255, w.x >> 24);
    b = int4(w.y & 255, (w.y >> 8) & 255, (w.y >> 16) & 255, w.y >> 24);
  }
}

void store_chroma4(int x, int y, uint4 u, uint4 v) {
  [unroll] for (int i = 0; i < 4; ++i) {
    sp_chroma[int2(x + i, y)] = uint2(u[i], v[i]);
  }
}

// Groups copy 128 samples by four rows. Below the luma groups, neighboring groups in X copy
// two successive UV tiles in Y. This fits half-width chroma in the same dispatch with at most
// one extra column of luma groups and one extra row of chroma groups.
[numthreads(16, 4, 1)] void main(uint3 group : SV_GroupID, uint3 lane : SV_GroupThreadID) {
  const bool luma = group.y < cb_luma_group_rows;
  const uint column = luma ? group.x : group.x >> 1;
  const uint row = luma ? group.y : 2 * (group.y - cb_luma_group_rows) + (group.x & 1);
  const uint x = column * 128 + lane.x * 8;
  const uint y = row * 4 + lane.y;
  const uint width = luma ? cb_width : (cb_width + 1) >> 1;
  const uint height = luma ? cb_height : (cb_height + 1) >> 1;
  if (x >= width || y >= height) return;

  const int shift = cb_hbd ? 6 : 0;
  // The source rows have right padding. The texture drops stores past the last texel.
  if (luma) {
    int4 a, b;
    load8(cb_y_offset + y * cb_y_stride, x, a, b);
    if ((width & 1) && x + 8 > width) {
      // Fill the extra column of the even-width texture from the last source sample.
      switch (width - x) {
        case 1: a.y = a.x; break;
        case 3: a.w = a.z; break;
        case 5: b.y = b.x; break;
        case 7: b.w = b.z; break;
      }
    }
    const uint4 lo = (uint4)a << shift;
    const uint4 hi = (uint4)b << shift;
    sp_store_luma_row(x, y, lo, cb_hbd, int2(0, 0));
    sp_store_luma_row(x + 4, y, hi, cb_hbd, int2(0, 0));
    if ((height & 1) && y == height - 1) {
      sp_store_luma_row(x, y + 1, lo, cb_hbd, int2(0, 0));
      sp_store_luma_row(x + 4, y + 1, hi, cb_hbd, int2(0, 0));
    }
  } else {
    const int neutral = cb_hbd ? 512 : 128;
    int4 ua = int4(neutral, neutral, neutral, neutral);
    int4 ub = ua, va = ua, vb = ua;
    if (!cb_neutral_chroma) {
      load8(cb_u_offset + y * cb_uv_stride, x, ua, ub);
      load8(cb_v_offset + y * cb_uv_stride, x, va, vb);
    }
    store_chroma4(x, y, (uint4)ua << shift, (uint4)va << shift);
    store_chroma4(x + 4, y, (uint4)ub << shift, (uint4)vb << shift);
  }
}
