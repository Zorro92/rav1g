/*
 * Copyright 2020 Google LLC
 *
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
#define InterOffsetBits 21
#define InterFilterLine filter_line_hbd
#define InterLineBias (FilterLineAdd10bit >> FilterLineShift)
#else
#define InterSampleBytes 1
#define InterPixelMax 255
#define InterOffsetBits 19
#define InterFilterLine filter_line
#define InterLineBias (FilterLineAdd8bit >> FilterLineShift)
#endif

#ifdef INTER_COMPOUND
// Each reference is rounded to 1/16 sample precision for the blend.
#define InterCompoundShift 7
#define InterCompoundOffset (1 << (InterOffsetBits - InterCompoundShift))
#define InterCompoundBias (InterCompoundOffset + InterLineBias)
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

// All lanes of a block share the phases. Lanes in a mixed wave can take
// different paths without exchanging samples.
InterPrediction inter_predict(int x, int y, int mv, uint flags, int refplane, int2 dims) {
  const int stride = cb_refplanes[refplane].x;
  const int ref_x = x + (mv >> (16 + SUBPEL_BITS)) - 3;
  const int ref_y = y + ((int)((uint)mv << 16) >> (16 + SUBPEL_BITS)) - 3;
  // Clamp the window origin before moving to tap 3. Horizontal edge samples
  // are supplied by the reference's extended border.
  const int column = cb_refplanes[refplane].y + clamp(ref_x, -11, dims.x) * InterSampleBytes;
  const int center = column + 3 * InterSampleBytes;
  const int phase_x = (mv >> 16) & SUBPEL_MASK;
  const int phase_y = mv & SUBPEL_MASK;
  const int filter_h = (((flags >> 5) & 15) << SUBPEL_BITS) + phase_x;
  const int filter_v = (((flags >> 9) & 15) << SUBPEL_BITS) + phase_y;

  InterPrediction prediction;
  [branch] if (phase_y == 0) {
    [branch] if (phase_x == 0) {
      [unroll] for (int i = 0; i < InterRows; ++i) {
        const int4 row = inter_load_row(center + stride * clamp(ref_y + 3 + i, 0, dims.y));
#ifdef INTER_COMPOUND
        prediction.row[i] = InterCompoundBias + row * 16;
#else
        prediction.row[i] = clamp(row, 0, InterPixelMax);
#endif
      }
    } else {
      // The identity vertical tap leaves four bits after the horizontal round.
      const int4 h0 = cb_kernels[filter_h][0];
      const int4 h1 = cb_kernels[filter_h][1];
      [unroll] for (int i = 0; i < InterRows; ++i) {
        const int4 row = InterFilterLine(dst_frame, column + stride * clamp(ref_y + 3 + i, 0, dims.y), h0, h1);
#ifdef INTER_COMPOUND
        prediction.row[i] = InterCompoundOffset + row;
#else
        prediction.row[i] = clamp(((8 - InterLineBias) + row) >> 4, 0, InterPixelMax);
#endif
      }
    }
  } else {
    const int4 v0 = cb_kernels[filter_v][0];
    const int4 v1 = cb_kernels[filter_v][1];
    [branch] if (phase_x == 0) {
      const int4 unused = int4(0, 0, 0, 0);
      prediction = inter_vertical(center, stride, ref_y, dims.y, unused, unused, v0, v1, false);
      // The identity horizontal tap scales samples by 16. The vertical taps
      // sum to 128, so the line bias is constant after the vertical round.
      [unroll] for (int i = 0; i < InterRows; ++i) {
#ifdef INTER_COMPOUND
        prediction.row[i] = InterCompoundBias + ((4 + prediction.row[i]) >> 3);
#else
        prediction.row[i] = clamp((64 + prediction.row[i]) >> 7, 0, InterPixelMax);
#endif
      }
    } else {
      prediction = inter_vertical(column, stride, ref_y, dims.y, cb_kernels[filter_h][0], cb_kernels[filter_h][1],
                                  v0, v1, true);
      [unroll] for (int i = 0; i < InterRows; ++i) {
#ifdef INTER_COMPOUND
        prediction.row[i] = ((64 + (1 << InterOffsetBits)) + prediction.row[i]) >> InterCompoundShift;
#else
        // Cancel the legacy offsets before the final shift to sample precision.
        prediction.row[i] = clamp((1024 - 128 * InterLineBias + prediction.row[i]) >> 11, 0, InterPixelMax);
#endif
      }
    }
  }
  return prediction;
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
