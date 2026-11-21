/*
 * SPDX-License-Identifier: BSD-2-Clause
 *
 * This source code is subject to the terms of the BSD 2 Clause License and the Alliance for Open Media
 * Patent License 1.0. If the BSD 2 Clause License was not distributed with this source code in the LICENSE
 * file, you can obtain it at www.aomedia.org/license/software. If the Alliance for Open Media Patent
 * License 1.0 was not distributed with this source code in the PATENTS file, you can obtain it at
 * www.aomedia.org/license/patent.
 */

// The planes of a frame that get no film grain, into the semi-planar output (output_sp.h): the luma
// plane, or the chroma plane from the U and the V plane of the frame, or a chroma plane of the middle
// value for a frame that has none. A thread writes eight samples of a row, or eight of U and of V.

RWByteAddressBuffer pool : register(u0);

#define SP_BUFFER pool
#include "output_sp.h"

#define SP_MODE_LUMA 0
#define SP_MODE_CHROMA 1
#define SP_MODE_CHROMA_NEUTRAL 2

cbuffer cb_copy_sp : register(b0) {
  uint cb_rows;    // of the plane that is written
  uint cb_groups;  // threads of a row
  uint cb_mode;
  uint cb_hbd;
  uint cb_src_offset;    // the luma plane, or U
  uint cb_src_offset_v;  // V
  uint cb_src_stride;
  uint cb_dst_offset;
  uint cb_dst_stride;
  uint cb_width;  // of the frame, in luma samples
  uint cb_height;
};

// Eight samples of a row of a plane of the frame, from the column `x` on, in two fours.
void load8(uint offset, int x, int y, out int4 a, out int4 b) {
  if (cb_hbd) {
    const uint4 w = pool.Load4(offset + y * cb_src_stride + x * 2);
    a = int4(w.x & 0xffff, w.x >> 16, w.y & 0xffff, w.y >> 16);
    b = int4(w.z & 0xffff, w.z >> 16, w.w & 0xffff, w.w >> 16);
  } else {
    const uint2 w = pool.Load2(offset + y * cb_src_stride + x);
    a = int4(w.x & 255, (w.x >> 8) & 255, (w.x >> 16) & 255, w.x >> 24);
    b = int4(w.y & 255, (w.y >> 8) & 255, (w.y >> 16) & 255, w.y >> 24);
  }
}

// A thread: eight samples of a row of the plane, or eight of U and of V.
[numthreads(8, 8, 1)] void main(uint3 thread
                                : SV_DispatchThreadID) {
  if (thread.x >= cb_groups || thread.y >= cb_rows) return;

  const int x = thread.x * 8;
  const int y = thread.y;
  const int2 plane = int2(cb_dst_stride, cb_dst_offset);

  if (cb_mode == SP_MODE_LUMA) {
    int4 a, b;
    load8(cb_src_offset, x, y, a, b);
    sp_store_luma(x, y, a, cb_width, cb_height, cb_hbd, plane);
    sp_store_luma(x + 4, y, b, cb_width, cb_height, cb_hbd, plane);
  } else {
    const int cwidth = (cb_width + 1) >> 1;
    const int cheight = (cb_height + 1) >> 1;
    int4 ua, ub, va, vb;
    if (cb_mode == SP_MODE_CHROMA) {
      load8(cb_src_offset, x, y, ua, ub);
      load8(cb_src_offset_v, x, y, va, vb);
    } else {
      const int neutral = cb_hbd ? 512 : 128;
      ua = int4(neutral, neutral, neutral, neutral);
      ub = ua;
      va = ua;
      vb = ua;
    }
    sp_store_chroma(x, y, ua, va, cwidth, cheight, cb_hbd, plane);
    sp_store_chroma(x + 4, y, ub, vb, cwidth, cheight, cb_hbd, plane);
  }
}
