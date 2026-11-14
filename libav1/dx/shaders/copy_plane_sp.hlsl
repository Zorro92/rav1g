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
// value for a frame that has none. A thread writes four samples of a row, or four of U and of V.
//
// copy_plane_sp_tex.hlsl is this file with SP_TEXTURE: the planes are those of the application's texture.

RWByteAddressBuffer pool : register(u0);

#ifdef SP_TEXTURE
#define SP_LUMA_REGISTER u1
#define SP_CHROMA_REGISTER u2
#else
#define SP_BUFFER pool
#endif
#include "output_sp.h"

#define SP_MODE_LUMA 0
#define SP_MODE_CHROMA 1
#define SP_MODE_CHROMA_NEUTRAL 2

cbuffer cb_copy_sp : register(b0) {
  uint cb_wi_count;
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

int4 load4(uint offset, int x, int y) {
  int4 v;
  if (cb_hbd) {
    const uint2 w = pool.Load2(offset + y * cb_src_stride + x * 2);
    v = int4(w.x & 0xffff, w.x >> 16, w.y & 0xffff, w.y >> 16);
  } else {
    const uint w = pool.Load(offset + y * cb_src_stride + x);
    v = int4(w & 255, (w >> 8) & 255, (w >> 16) & 255, w >> 24);
  }
  return v;
}

[numthreads(64, 1, 1)] void main(uint3 thread
                                 : SV_DispatchThreadID) {
  if (thread.x >= cb_wi_count) return;

  const int x = (thread.x % cb_groups) * 4;
  const int y = thread.x / cb_groups;
  const int2 plane = int2(cb_dst_stride, cb_dst_offset);

  if (cb_mode == SP_MODE_LUMA) {
    sp_store_luma(x, y, load4(cb_src_offset, x, y), cb_width, cb_height, cb_hbd, plane);
  } else {
    const int cwidth = (cb_width + 1) >> 1;
    const int cheight = (cb_height + 1) >> 1;
    int4 u, v;
    if (cb_mode == SP_MODE_CHROMA) {
      u = load4(cb_src_offset, x, y);
      v = load4(cb_src_offset_v, x, y);
    } else {
      const int neutral = cb_hbd ? 512 : 128;
      u = int4(neutral, neutral, neutral, neutral);
      v = u;
    }
    sp_store_chroma(x, y, u, v, cwidth, cheight, cb_hbd, plane);
  }
}
