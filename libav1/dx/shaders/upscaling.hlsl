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

#pragma warning(disable : 3556)
#define Round2(value, n) (((value) + (((1 << (n)) >> 1))) >> (n))
#define SUPERRES_SCALE_BITS 14
#define SUPERRES_SCALE_MASK ((1 << 14) - 1)
#define SUPERRES_EXTRA_BITS 8
#define SUPERRES_FILTER_OFFSET 3
#define SUPERRES_FILTER_TAPS 8
#define SUPERRES_FILTER_BITS 6
#define SUPERRES_FILTER_SHIFTS (1 << SUPERRES_FILTER_BITS)
#define FILTER_BITS 7

RWByteAddressBuffer dst_frame : register(u0);

cbuffer UpscaleData : register(b0) {
  uint4 src_planes[3];
  uint4 dst_planes[3];
};

cbuffer UpscaleSRT : register(b1) {
  uint cb_plane;
  uint cb_wi_count;
  uint cb_hbd;
};

[numthreads(64, 1, 1)] void main(uint3 thread
                                 : SV_DispatchThreadID) {
  const int Upscale_Filter[SUPERRES_FILTER_SHIFTS][SUPERRES_FILTER_TAPS] = {
      {0, 0, 0, 128, 0, 0, 0, 0},        {0, 0, -1, 128, 2, -1, 0, 0},      {0, 1, -3, 127, 4, -2, 1, 0},
      {0, 1, -4, 127, 6, -3, 1, 0},      {0, 2, -6, 126, 8, -3, 1, 0},      {0, 2, -7, 125, 11, -4, 1, 0},
      {-1, 2, -8, 125, 13, -5, 2, 0},    {-1, 3, -9, 124, 15, -6, 2, 0},    {-1, 3, -10, 123, 18, -6, 2, -1},
      {-1, 3, -11, 122, 20, -7, 3, -1},  {-1, 4, -12, 121, 22, -8, 3, -1},  {-1, 4, -13, 120, 25, -9, 3, -1},
      {-1, 4, -14, 118, 28, -9, 3, -1},  {-1, 4, -15, 117, 30, -10, 4, -1}, {-1, 5, -16, 116, 32, -11, 4, -1},
      {-1, 5, -16, 114, 35, -12, 4, -1}, {-1, 5, -17, 112, 38, -12, 4, -1}, {-1, 5, -18, 111, 40, -13, 5, -1},
      {-1, 5, -18, 109, 43, -14, 5, -1}, {-1, 6, -19, 107, 45, -14, 5, -1}, {-1, 6, -19, 105, 48, -15, 5, -1},
      {-1, 6, -19, 103, 51, -16, 5, -1}, {-1, 6, -20, 101, 53, -16, 6, -1}, {-1, 6, -20, 99, 56, -17, 6, -1},
      {-1, 6, -20, 97, 58, -17, 6, -1},  {-1, 6, -20, 95, 61, -18, 6, -1},  {-2, 7, -20, 93, 64, -18, 6, -2},
      {-2, 7, -20, 91, 66, -19, 6, -1},  {-2, 7, -20, 88, 69, -19, 6, -1},  {-2, 7, -20, 86, 71, -19, 6, -1},
      {-2, 7, -20, 84, 74, -20, 7, -2},  {-2, 7, -20, 81, 76, -20, 7, -1},  {-2, 7, -20, 79, 79, -20, 7, -2},
      {-1, 7, -20, 76, 81, -20, 7, -2},  {-2, 7, -20, 74, 84, -20, 7, -2},  {-1, 6, -19, 71, 86, -20, 7, -2},
      {-1, 6, -19, 69, 88, -20, 7, -2},  {-1, 6, -19, 66, 91, -20, 7, -2},  {-2, 6, -18, 64, 93, -20, 7, -2},
      {-1, 6, -18, 61, 95, -20, 6, -1},  {-1, 6, -17, 58, 97, -20, 6, -1},  {-1, 6, -17, 56, 99, -20, 6, -1},
      {-1, 6, -16, 53, 101, -20, 6, -1}, {-1, 5, -16, 51, 103, -19, 6, -1}, {-1, 5, -15, 48, 105, -19, 6, -1},
      {-1, 5, -14, 45, 107, -19, 6, -1}, {-1, 5, -14, 43, 109, -18, 5, -1}, {-1, 5, -13, 40, 111, -18, 5, -1},
      {-1, 4, -12, 38, 112, -17, 5, -1}, {-1, 4, -12, 35, 114, -16, 5, -1}, {-1, 4, -11, 32, 116, -16, 5, -1},
      {-1, 4, -10, 30, 117, -15, 4, -1}, {-1, 3, -9, 28, 118, -14, 4, -1},  {-1, 3, -9, 25, 120, -13, 4, -1},
      {-1, 3, -8, 22, 121, -12, 4, -1},  {-1, 3, -7, 20, 122, -11, 3, -1},  {-1, 2, -6, 18, 123, -10, 3, -1},
      {0, 2, -6, 15, 124, -9, 3, -1},    {0, 2, -5, 13, 125, -8, 2, -1},    {0, 1, -4, 11, 125, -7, 2, 0},
      {0, 1, -3, 8, 126, -6, 2, 0},      {0, 1, -3, 6, 127, -4, 1, 0},      {0, 1, -2, 4, 127, -3, 1, 0},
      {0, 0, -1, 2, 128, -1, 0, 0},
  };

  if (thread.x >= cb_wi_count) return;
  uint4 dst_plane = dst_planes[cb_plane];
  uint4 src_plane = src_planes[cb_plane];
  const uint w = (dst_plane.z + 3) >> 2;
  const uint x = thread.x % w;
  const uint y = thread.x / w;

  const int downscaledPlaneW = src_plane.z;
  const int upscaledPlaneW = dst_plane.z;
  const int stepX = ((downscaledPlaneW << SUPERRES_SCALE_BITS) + (upscaledPlaneW >> 1)) / upscaledPlaneW;
  const int err = (upscaledPlaneW * stepX) - (downscaledPlaneW << SUPERRES_SCALE_BITS);
  int initialSubpelX =
      (-((upscaledPlaneW - downscaledPlaneW) << (SUPERRES_SCALE_BITS - 1)) + (upscaledPlaneW >> 1)) / upscaledPlaneW +
      (1 << (SUPERRES_EXTRA_BITS - 1)) - err / 2;
  initialSubpelX &= SUPERRES_SCALE_MASK;
  int minX = 0;
  int maxX = src_plane.w - 1;
  // for (int y = 0; y < planeH; y++ ) {
  // for (int x = 0; x < upscaledPlaneW; x+=4 ) {
  int4 srcX;
  srcX.x = -(1 << SUPERRES_SCALE_BITS) + initialSubpelX + (x * 4 + 0) * stepX;
  srcX.y = -(1 << SUPERRES_SCALE_BITS) + initialSubpelX + (x * 4 + 1) * stepX;
  srcX.z = -(1 << SUPERRES_SCALE_BITS) + initialSubpelX + (x * 4 + 2) * stepX;
  srcX.w = -(1 << SUPERRES_SCALE_BITS) + initialSubpelX + (x * 4 + 3) * stepX;
  int4 srcXPx = (srcX >> SUPERRES_SCALE_BITS);
  int4 srcXSubpel = (srcX & SUPERRES_SCALE_MASK) >> SUPERRES_EXTRA_BITS;
  int4 sum = 0;
  int first_x = (srcXPx.x - SUPERRES_FILTER_OFFSET) >> 2;
  int samples[16];
  for (int i = 0; i < 4; i++) {
    if (cb_hbd) {
      if (first_x + i < minX) {
        uint pixels = dst_frame.Load(src_plane.y + y * src_plane.x);
        samples[i * 4 + 0] = pixels & 0x3ff;
        samples[i * 4 + 1] = pixels & 0x3ff;
        samples[i * 4 + 2] = pixels & 0x3ff;
        samples[i * 4 + 3] = pixels & 0x3ff;
      } else if (first_x + i > (maxX >> 2)) {
        uint2 pixels = dst_frame.Load2(src_plane.y + ((maxX >> 2) << 3) + y * src_plane.x);
        samples[i * 4 + 0] = (pixels.y >> 16) & 0x3ff;
        samples[i * 4 + 1] = (pixels.y >> 16) & 0x3ff;
        samples[i * 4 + 2] = (pixels.y >> 16) & 0x3ff;
        samples[i * 4 + 3] = (pixels.y >> 16) & 0x3ff;
      } else {
        uint2 pixels = dst_frame.Load2(src_plane.y + (first_x + i) * 8 + y * src_plane.x);
        samples[i * 4 + 0] = (pixels.x >> 0) & 0x3ff;
        samples[i * 4 + 1] = (pixels.x >> 16) & 0x3ff;
        samples[i * 4 + 2] = (pixels.y >> 0) & 0x3ff;
        samples[i * 4 + 3] = (pixels.y >> 16) & 0x3ff;
      }
    } else {
      if (first_x + i < minX) {
        uint pixels = dst_frame.Load(src_plane.y + y * src_plane.x);
        samples[i * 4 + 0] = pixels & 0xff;
        samples[i * 4 + 1] = pixels & 0xff;
        samples[i * 4 + 2] = pixels & 0xff;
        samples[i * 4 + 3] = pixels & 0xff;
      } else if (first_x + i > (maxX >> 2)) {
        uint pixels = dst_frame.Load(src_plane.y + ((maxX >> 2) << 2) + y * src_plane.x);
        samples[i * 4 + 0] = (pixels >> 24) & 0xff;
        samples[i * 4 + 1] = (pixels >> 24) & 0xff;
        samples[i * 4 + 2] = (pixels >> 24) & 0xff;
        samples[i * 4 + 3] = (pixels >> 24) & 0xff;
      } else {
        uint pixels = dst_frame.Load(src_plane.y + (first_x + i) * 4 + y * src_plane.x);
        samples[i * 4 + 0] = (pixels >> 0) & 0xff;
        samples[i * 4 + 1] = (pixels >> 8) & 0xff;
        samples[i * 4 + 2] = (pixels >> 16) & 0xff;
        samples[i * 4 + 3] = (pixels >> 24) & 0xff;
      }
    }
  }
  for (int k = 0; k < SUPERRES_FILTER_TAPS; k++) {
    int4 sampleX = srcXPx + (k - SUPERRES_FILTER_OFFSET) - (first_x << 2);
    sum.x += samples[sampleX.x] * Upscale_Filter[srcXSubpel.x][k];
    sum.y += samples[sampleX.y] * Upscale_Filter[srcXSubpel.y][k];
    sum.z += samples[sampleX.z] * Upscale_Filter[srcXSubpel.z][k];
    sum.w += samples[sampleX.w] * Upscale_Filter[srcXSubpel.w][k];
  }
  sum = clamp(Round2(sum, FILTER_BITS), 0, cb_hbd ? 1023 : 255);

  if (cb_hbd) {
    dst_frame.Store2(dst_plane.y + x * 8 + y * dst_plane.x, uint2(sum.x | (sum.y << 16), sum.z | (sum.w << 16)));
  } else {
    uint res = (sum.x << 0) | (sum.y << 8) | (sum.z << 16) | (sum.w << 24);
    dst_frame.Store(dst_plane.y + x * 4 + y * dst_plane.x, res);
  }
}
