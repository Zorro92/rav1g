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

#include "inter_common.h"

#ifdef INTER_HBD
#define InterSampleBytes 2
#define InterPixelMax 1023
#define InterFilterLine filter_line_hbd
#define InterLineBias (FilterLineAdd10bit >> FilterLineShift)
#else
#define InterSampleBytes 1
#define InterPixelMax 255
#define InterFilterLine filter_line
#define InterLineBias (FilterLineAdd8bit >> FilterLineShift)
#endif

struct InterPrediction {
  int4 row[InterRows];
};

// Four samples starting at tap 3 when the horizontal filter is the identity.
// Load the aligned words covering them even when the first sample is unaligned.
int4 inter_load_row(int offset) {
  const int shift = (offset & 3) * 8;
#ifdef INTER_HBD
  const uint3 words = dst_frame.Load3(offset & ~3);
  const uint lo = (words.x >> shift) | ((words.y << (24 - shift)) << 8);
  const uint hi = (words.y >> shift) | ((words.z << (24 - shift)) << 8);
  return int4(lo & 0xffff, lo >> 16, hi & 0xffff, hi >> 16);
#else
  const uint2 words = dst_frame.Load2(offset & ~3);
  const uint packed = (words.x >> shift) | ((words.y << (24 - shift)) << 8);
  return int4(packed & 0xff, (packed >> 8) & 0xff, (packed >> 16) & 0xff, packed >> 24);
#endif
}

// One load/filter per input row, reused across the output rows whose eight-tap
// windows overlap it. Unroll both loops to keep just the output accumulators
// and the current input row live. The callers specialize horizontal at compile time.
InterPrediction inter_vertical(int column, int stride, int y, int height, int4 h0, int4 h1, int4 v0, int4 v1,
                               bool horizontal) {
  const int taps[8] = {v0.x, v0.y, v0.z, v0.w, v1.x, v1.y, v1.z, v1.w};
  InterPrediction result;
  [unroll] for (int i = 0; i < InterRows; ++i) {
    result.row[i] = int4(0, 0, 0, 0);
  }
  [unroll] for (int input_row = 0; input_row < InterRows + 7; ++input_row) {
    const int address = column + stride * clamp(y + input_row, 0, height);
    int4 row;
    if (horizontal) {
      row = InterFilterLine(dst_frame, address, h0, h1);
    } else {
      row = inter_load_row(address);
    }
    [unroll] for (int output_row = 0; output_row < InterRows; ++output_row) {
      const int tap = input_row - output_row;
      if (tap >= 0 && tap < 8) {
        result.row[output_row] += row * taps[tap];
      }
    }
  }
  return result;
}

int4 inter_add_residual(int offset, int4 row) {
  const int2 r = (int2)residuals.Load2(offset);
  row.x += (int)((uint)r.x << 16) >> 16;
  row.y += r.x >> 16;
  row.z += (int)((uint)r.y << 16) >> 16;
  row.w += r.y >> 16;
  return clamp(row, 0, InterPixelMax);
}

void inter_store_row(int offset, int4 row) {
#ifdef INTER_HBD
  dst_frame.Store2(offset, uint2(row.x | (row.y << 16), row.z | (row.w << 16)));
#else
  dst_frame.Store(offset, row.x | (row.y << 8) | (row.z << 16) | (row.w << 24));
#endif
}
