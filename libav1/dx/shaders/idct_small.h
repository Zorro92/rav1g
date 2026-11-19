/*
 * SPDX-License-Identifier: BSD-2-Clause
 *
 * This source code is subject to the terms of the BSD 2 Clause License and the Alliance for Open Media
 * Patent License 1.0. If the BSD 2 Clause License was not distributed with this source code in the LICENSE
 * file, you can obtain it at www.aomedia.org/license/software. If the Alliance for Open Media Patent
 * License 1.0 was not distributed with this source code in the PATENTS file, you can obtain it at
 * www.aomedia.org/license/patent.
 */

// The identity pass must also have only constant indices into the lane's samples.
#define IDCT_IDENTITY_LOOP [unroll]
#include "idct_shader_common.h"

// Default scans from av1/common/scan.c. The other two scans are row-major and column-major.
// The unrolled scatter below uses constant array indices, so the coefficients can stay in registers.
static const int DefaultScan[TX_WIDTH * TX_HEIGHT] = {
#if TX_WIDTH == 4 && TX_HEIGHT == 4
  0, 1, 4, 8, 5, 2, 3, 6, 9, 12, 13, 10, 7, 11, 14, 15
#elif TX_WIDTH == 4 && TX_HEIGHT == 8
  0,  1,  4,  2,  5,  8,  3,  6,  9,  12, 7,  10, 13, 16, 11, 14,
  17, 20, 15, 18, 21, 24, 19, 22, 25, 28, 23, 26, 29, 27, 30, 31
#elif TX_WIDTH == 8 && TX_HEIGHT == 4
  0,  8, 1,  16, 9,  2, 24, 17, 10, 3, 25, 18, 11, 4,  26, 19,
  12, 5, 27, 20, 13, 6, 28, 21, 14, 7, 29, 22, 15, 30, 23, 31
#elif TX_WIDTH == 8 && TX_HEIGHT == 8
  0,  1,  8,  16, 9,  2,  3,  10, 17, 24, 32, 25, 18, 11, 4,  5,
  12, 19, 26, 33, 40, 48, 41, 34, 27, 20, 13, 6,  7,  14, 21, 28,
  35, 42, 49, 56, 57, 50, 43, 36, 29, 22, 15, 23, 30, 37, 44, 51,
  58, 59, 52, 45, 38, 31, 39, 46, 53, 60, 61, 54, 47, 55, 62, 63
#endif
};

#define IDENTITY4(input, in_offset, in_stride, output, out_offset, out_stride, range, output_shift) \
  IIDENTITY_N(4, NewSqrt2, NewSqrt2Bits, input, in_offset, in_stride, output, out_offset, out_stride, range, output_shift)
#define IDENTITY8(input, in_offset, in_stride, output, out_offset, out_stride, range, output_shift) \
  IIDENTITY_N(8, 2, 0, input, in_offset, in_stride, output, out_offset, out_stride, range, output_shift)

#if TX_WIDTH == 4
#define ROW_DCT IDCT4
#define ROW_ADST IADST4
#define ROW_IDENTITY IDENTITY4
#else
#define ROW_DCT IDCT8
#define ROW_ADST IADST8
#define ROW_IDENTITY IDENTITY8
#endif

#if TX_HEIGHT == 4
#define COL_DCT IDCT4
#define COL_ADST IADST4
#define COL_IDENTITY IDENTITY4
#else
#define COL_DCT IDCT8
#define COL_ADST IADST8
#define COL_IDENTITY IDENTITY8
#endif

#define TRANSFORM_LINES(op, lines, step, stride, range, shift) \
  [unroll] for (int line_idx = 0; line_idx < lines; ++line_idx) { \
    const int base = line_idx * step; \
    op(samples, base, stride, samples, base, stride, range, shift); \
  }

#define TRANSFORM_PASS(type, dct, adst, identity, lines, step, stride, range, shift) \
  [branch] if (type == 0) { \
    TRANSFORM_LINES(dct, lines, step, stride, range, shift); \
  } else if (type == 1) { \
    TRANSFORM_LINES(adst, lines, step, stride, range, shift); \
  } else { \
    TRANSFORM_LINES(identity, lines, step, stride, range, shift); \
  }

int scale_coefficient(int value, int2 range) {
  value = clamp_value(value, range);
  if (TX_WIDTH != TX_HEIGHT) value = round_shift(value * NewInvSqrt2, NewSqrt2Bits);
  return value;
}

uint2 pack_residual4(int4 v, bool flip_x) {
  if (flip_x) v.xyzw = v.wzyx;
  return uint2((v.x & 0xffff) | (v.y << 16), (v.z & 0xffff) | (v.w << 16));
}

// One lane owns the block record, all coefficient words and all residual writes.
[numthreads(64, 1, 1)] void main(uint3 thread : SV_DispatchThreadID) {
  if (thread.x >= cb_wicount) return;
  const uint4 block = buf_blocks.Load4((cb_index_offset + thread.x) * IdctBlockSize);
  const uint coef_count = block.x & 0x7ff;
  const uint scan = (block.x >> ScanShift) & ScanMask;
  const int2 row_range = int2(-(1 << (cb_bitdepth + 7)), (1 << (cb_bitdepth + 7)) - 1);
  const int2 col_range = int2(ClampMinValue16, ClampMaxValue16);
  const int row_shift = TX_WIDTH == 8 && TX_HEIGHT == 8 ? 1 : 0;

  int coefs[TX_WIDTH * TX_HEIGHT];
  [unroll] for (uint i = 0; i < TX_WIDTH * TX_HEIGHT / 4; ++i) {
    int4 word = int4(0, 0, 0, 0);
    [branch] if (i < coef_count) {
      const uint address = block.y * 4 + i * 16;
      word = (int4)buf_input.Load4(address);
      buf_input.Store4(address, uint4(0, 0, 0, 0));
    }
    coefs[4 * i + 0] = scale_coefficient(word.x, row_range);
    coefs[4 * i + 1] = scale_coefficient(word.y, row_range);
    coefs[4 * i + 2] = scale_coefficient(word.z, row_range);
    coefs[4 * i + 3] = scale_coefficient(word.w, row_range);
  }

  int samples[TX_WIDTH * TX_HEIGHT];
  [branch] if (scan == 0) {
    [unroll] for (int i = 0; i < TX_WIDTH * TX_HEIGHT; ++i) samples[DefaultScan[i]] = coefs[i];
  } else if (scan == 1) {
    [unroll] for (int i = 0; i < TX_WIDTH * TX_HEIGHT; ++i) samples[i] = coefs[i];
  } else {
    [unroll] for (int i = 0; i < TX_WIDTH * TX_HEIGHT; ++i) {
      samples[(i % TX_HEIGHT) * TX_WIDTH + i / TX_HEIGHT] = coefs[i];
    }
  }

  const uint horizontal = (block.x >> IdctType_H_Shift) & 3;
  const uint vertical = (block.x >> IdctType_V_Shift) & 3;
  TRANSFORM_PASS(horizontal, ROW_DCT, ROW_ADST, ROW_IDENTITY, TX_HEIGHT, TX_WIDTH, 1, row_range, row_shift);
  TRANSFORM_PASS(vertical, COL_DCT, COL_ADST, COL_IDENTITY, TX_WIDTH, 1, TX_WIDTH, col_range, 4);

  const bool flip_x = (block.x & IdctType_H_Flip) != 0;
  const bool flip_y = (block.x & IdctType_V_Flip) != 0;
  const uint plane = (block.x >> 21) & 3;
  const uint stride = cb_planes[plane].x;
  const uint origin = cb_planes[plane].y + 8 * (block.z & 0xffff) + 4 * (block.z >> 16) * stride;
  [unroll] for (uint y = 0; y < TX_HEIGHT; ++y) {
    const uint row = origin + (flip_y ? TX_HEIGHT - 1 - y : y) * stride;
    const uint i = y * TX_WIDTH;
    const uint2 lo = pack_residual4(int4(samples[i], samples[i + 1], samples[i + 2], samples[i + 3]), flip_x);
#if TX_WIDTH == 8
    const uint2 hi = pack_residual4(int4(samples[i + 4], samples[i + 5], samples[i + 6], samples[i + 7]), flip_x);
    const uint2 left = flip_x ? hi : lo;
    const uint2 right = flip_x ? lo : hi;
    buf_dst.Store4(row, uint4(left.x, left.y, right.x, right.y));
#else
    buf_dst.Store2(row, lo);
#endif
  }
}
