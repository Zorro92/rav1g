/*
 * SPDX-License-Identifier: BSD-2-Clause
 *
 * This source code is subject to the terms of the BSD 2 Clause License and the Alliance for Open Media
 * Patent License 1.0. If the BSD 2 Clause License was not distributed with this source code in the LICENSE
 * file, you can obtain it at www.aomedia.org/license/software. If the Alliance for Open Media Patent
 * License 1.0 was not distributed with this source code in the PATENTS file, you can obtain it at
 * www.aomedia.org/license/patent.
 */

// The stores of the semi-planar output (docs/output.md): a luma plane and one chroma plane with U and V
// side by side, NV12 for 8-bit frames and P010 for 10-bit frames (the 10 bits in the high bits of 16).
//
// SP_TEXTURE: the planes are the two plane slices of the application's NV12 or P010 texture, at the
// registers SP_LUMA_REGISTER and SP_CHROMA_REGISTER. Without it they lie in the buffer SP_BUFFER, rows a
// stride apart, as that texture is laid out in memory.
//
// The planes have the size of the texture: the frame size with both rounded up to even. A frame of odd
// width has one more luma column, a frame of odd height one more luma row; they repeat the last column
// and the last row of the frame. The chroma plane of such a frame is whole without them.

#ifdef SP_TEXTURE
RWTexture2D<uint> sp_luma : register(SP_LUMA_REGISTER);
RWTexture2D<uint2> sp_chroma : register(SP_CHROMA_REGISTER);
#endif

// Four luma samples of the row `py` from the column `px` on, already shifted. `plane`: stride and offset.
void sp_store_luma_row(int px, int py, uint4 s, int hbd, int2 plane) {
#ifdef SP_TEXTURE
  sp_luma[int2(px + 0, py)] = s.x;
  sp_luma[int2(px + 1, py)] = s.y;
  sp_luma[int2(px + 2, py)] = s.z;
  sp_luma[int2(px + 3, py)] = s.w;
#else
  const uint row = plane.y + py * plane.x;
  if (hbd) {
    SP_BUFFER.Store2(row + px * 2, uint2(s.x | (s.y << 16), s.z | (s.w << 16)));
  } else {
    SP_BUFFER.Store(row + px, s.x | (s.y << 8) | (s.z << 16) | (s.w << 24));
  }
#endif
}

// Four luma samples of the frame, at the row `py` from the column `px` (a multiple of 4) on. `width` and
// `height` are those of the frame. Samples to the right of the plane are dropped by a texture and fall
// into the padding of a row in a buffer.
void sp_store_luma(int px, int py, int4 v, int width, int height, int hbd, int2 plane) {
  if (py >= height || px >= width) return;
  if (width & 1) {
    // The column behind the last one of a frame of odd width.
    const int k = width - px;
    if (k == 1) v.y = v.x;
    if (k == 3) v.w = v.z;
  }
  const uint4 s = (uint4)v << (hbd ? 6 : 0);
  sp_store_luma_row(px, py, s, hbd, plane);
  if ((height & 1) && py == height - 1) {
    // The row behind the last one of a frame of odd height.
    sp_store_luma_row(px, py + 1, s, hbd, plane);
  }
}

// Four chroma samples of U and of V, at the row `cy` from the column `cx` (a multiple of 4) on, of a
// chroma plane of `cwidth` by `cheight` samples each.
void sp_store_chroma(int cx, int cy, int4 cb, int4 cr, int cwidth, int cheight, int hbd, int2 plane) {
  if (cy >= cheight || cx >= cwidth) return;
  const uint4 u = (uint4)cb << (hbd ? 6 : 0);
  const uint4 v = (uint4)cr << (hbd ? 6 : 0);
#ifdef SP_TEXTURE
  sp_chroma[int2(cx + 0, cy)] = uint2(u.x, v.x);
  sp_chroma[int2(cx + 1, cy)] = uint2(u.y, v.y);
  sp_chroma[int2(cx + 2, cy)] = uint2(u.z, v.z);
  sp_chroma[int2(cx + 3, cy)] = uint2(u.w, v.w);
#else
  const uint row = plane.y + cy * plane.x;
  if (hbd) {
    SP_BUFFER.Store4(row + cx * 4, uint4(u.x | (v.x << 16), u.y | (v.y << 16), u.z | (v.z << 16), u.w | (v.w << 16)));
  } else {
    SP_BUFFER.Store2(row + cx * 2, uint2(u.x | (v.x << 8) | (u.y << 16) | (v.y << 24),
                                         u.z | (v.z << 8) | (u.w << 16) | (v.w << 24)));
  }
#endif
}
