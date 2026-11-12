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

// What the fast inter shaders share (test flag -interfast; docs/gpu-av1-decoder.md, "Inter prediction:
// breakdown and fast paths"). Included behind inter_common.h; INTER_FAST_HBD selects the build for frame
// buffers of 16-bit samples.
//
// A reference fetch has one of four classes, by the fraction of its motion vector:
//   0 copy   no fraction: the samples themselves
//   1 h      a fraction in x only: one 8-tap pass along the row
//   2 v      a fraction in y only: one 8-tap pass down the column
//   3 2d     both: the two passes of the original shaders
// The host sorts the blocks into one list per class (gen_pred_blocks_fast.hlsl places the records) and hands
// the class of the list to the shader with the root constants: bits 8 and up of cb_width_log2. Every thread
// of a dispatch therefore takes the same path.
//
// The original shaders run both passes for every block. With the kernel of fraction 0, which is
// {0, 0, 0, 128, 0, 0, 0, 0} for every filter type, a pass only scales the sample: filter_line() returns
// 16 * p + (1 << 14 or 16) / 8, and the vertical sum of such rows is 128 times the row. The classes 0 to 2
// leave those passes out and add what their rounding leaves in the result. testing/inter_check compares
// every class with the original shaders and with libaom for all fractions, kernels and sample values.

#define InterFastCopy 0
#define InterFastH 1
#define InterFastV 2
#define InterFast2D 3

#ifdef INTER_FAST_HBD
#define InterFastPixelMax 1023
#define InterFastSampleShift 1
// filter_line_hbd() returns ((sum + 4) >> 3) + InterFastLineBias.
#define InterFastLineBias (1 << (16 - FilterLineShift))
#define InterFastLine filter_line_hbd
#else
#define InterFastPixelMax 255
#define InterFastSampleShift 0
// filter_line() returns ((sum + 4) >> 3) + InterFastLineBias.
#define InterFastLineBias (1 << (14 - FilterLineShift))
#define InterFastLine filter_line
#endif

// Four samples of a row, the first at the byte |offset|: the loads of filter_line() cut down to the samples
// a pass without horizontal taps uses.
#ifdef INTER_FAST_HBD
int4 inter_fast_row(RWByteAddressBuffer ref, int offset) {
  // The row starts at the first or at the second sample of a word. Two loads of two words, one word apart,
  // so that every word of the row is put together from the same component of the two: shifting three
  // loaded words in place, as filter_line_hbd() does, compiles here to the pair of instructions
  // testing/dxbc_scan refuses (a paired read before an overlapping in-place write).
  const int shift = 8 * (offset & 2);
  const int base = offset & (~3);
  const uint2 lo = ref.Load2(base);
  const uint2 hi = ref.Load2(base + 4);
  uint2 p;
  p.x = (lo.x >> shift) | ((hi.x << (24 - shift)) << 8);
  p.y = (lo.y >> shift) | ((hi.y << (24 - shift)) << 8);
  int4 row;
  row.x = (int)(p.x & 0xffff);
  row.y = (int)(p.x >> 16);
  row.z = (int)(p.y & 0xffff);
  row.w = (int)(p.y >> 16);
  return row;
}
#else
int4 inter_fast_row(RWByteAddressBuffer ref, int offset) {
  const uint shift = (offset & 3) * 8;
  uint2 l = ref.Load2(offset & (~3));
  l.x = (l.x >> shift) | ((l.y << (24 - shift)) << 8);
  int4 row;
  row.x = (int)(l.x & 0xff);
  row.y = (int)((l.x >> 8) & 0xff);
  row.z = (int)((l.x >> 16) & 0xff);
  row.w = (int)(l.x >> 24);
  return row;
}
#endif

// One output row of a vertical 8-tap pass over the rows r0..r7: the plain sum of taps times samples.
int4 inter_fast_taps(int4 r0, int4 r1, int4 r2, int4 r3, int4 r4, int4 r5, int4 r6, int4 r7, int4 k0, int4 k1) {
  int4 sum;
  sum.x = r0.x * k0.x + r1.x * k0.y + r2.x * k0.z + r3.x * k0.w + r4.x * k1.x + r5.x * k1.y + r6.x * k1.z + r7.x * k1.w;
  sum.y = r0.y * k0.x + r1.y * k0.y + r2.y * k0.z + r3.y * k0.w + r4.y * k1.x + r5.y * k1.y + r6.y * k1.z + r7.y * k1.w;
  sum.z = r0.z * k0.x + r1.z * k0.y + r2.z * k0.z + r3.z * k0.w + r4.z * k1.x + r5.z * k1.y + r6.z * k1.z + r7.z * k1.w;
  sum.w = r0.w * k0.x + r1.w * k0.y + r2.w * k0.z + r3.w * k0.w + r4.w * k1.x + r5.w * k1.y + r6.w * k1.z + r7.w * k1.w;
  return sum;
}

// (v + add) >> shift of each component, limited to the sample range.
int4 inter_fast_round(int4 v, int add, int shift) {
  int4 r;
  r.x = clamp((v.x + add) >> shift, 0, InterFastPixelMax);
  r.y = clamp((v.y + add) >> shift, 0, InterFastPixelMax);
  r.z = clamp((v.z + add) >> shift, 0, InterFastPixelMax);
  r.w = clamp((v.w + add) >> shift, 0, InterFastPixelMax);
  return r;
}
