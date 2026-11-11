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

#define FILTER_MASK 3
#define SUBPEL_BITS 4
#define SUBPEL_MASK ((1 << SUBPEL_BITS) - 1)
#define BLOCK_POS_MASK 0x7fff
#define SCALE_BITS 14
#define REF_SCALE_SHIFT 14
#define SCALE_SUBPEL_BITS 10
#define SCALE_SUBPEL_SHIFTS (1 << SCALE_SUBPEL_BITS)        // 1024
#define SCALE_SUBPEL_MASK (SCALE_SUBPEL_SHIFTS - 1)         // 1023
#define SCALE_EXTRA_BITS (SCALE_SUBPEL_BITS - SUBPEL_BITS)  // 6
#define SCALE_EXTRA_OFF ((1 << SCALE_EXTRA_BITS) / 2)       // 32
#define NON_SQR_FLAG_SHIFT_X 15
#define NON_SQR_FLAG_SHIFT_Y 31
#define RefCount 7
#define NoSkipFlag 0x2000

#define FilterLineShift 3
#define FilterLineAdd8bit ((1 << (FilterLineShift - 1)) + (1 << 14))
#define FilterLineAdd10bit ((1 << (FilterLineShift - 1)) + (1 << 16))

#define WarpHorizBits 3
#define WarpHorizSumAdd ((1 << 14) + (1 << (WarpHorizBits - 1)))
#define WarpHorizSumAdd10 ((1 << 16) + (1 << (WarpHorizBits - 1)))
#define PixelMax10 1023
#define WarpPrecBits 16
#define WarpReduceBits 6
#define WarpFiltRoundBits (WarpPrecBits - WarpReduceBits)
#define WarpFiltRoundAdd (1 << (WarpFiltRoundBits - 1))
#define WarpFiltOffset 64
#define WarpFilterSize 32
#define WarpBlockSize 48

ByteAddressBuffer pred_blocks : register(t0);
ByteAddressBuffer residuals : register(t1);
ByteAddressBuffer comp_mask : register(t2);
ByteAddressBuffer warp_blocks : register(t3);
ByteAddressBuffer warp_filter : register(t4);
RWByteAddressBuffer dst_frame : register(u0);

typedef struct {
  int4 info0;
  int4 info1;
  int4 info2;
} GlobalMotionWarp;

cbuffer PSSLInterData : register(b0) {
  int4 cb_planes[3];
  int4 cb_refplanes[3 * RefCount];
  int4 cb_dims[2];
  int4 cb_pixel_max;
  int4 cb_kernels[8 * 16][2];
  int4 cb_scale[8];
  int4 cb_obmc_mask[1 + 1 + 2 + 4 + 8];
  GlobalMotionWarp cb_gm_warp[RefCount];
};

cbuffer PSSLInterSRT : register(b1) {
  uint cb_wi_count;
  uint cb_pass_offset;
  uint cb_width_log2;
  uint cb_height_log2;
};

int4 filter_line(RWByteAddressBuffer ref, int offset, int4 fkernel0, int4 fkernel1) {
  uint4 l = ref.Load4(offset & (~3));
  uint shift = (offset & 3) * 8;
  l.x = (l.x >> shift) | ((l.y << (24 - shift)) << 8);
  l.y = (l.y >> shift) | ((l.z << (24 - shift)) << 8);
  l.z = (l.z >> shift) | ((l.w << (24 - shift)) << 8);

  int4 sum = {0, 0, 0, 0};
  int filter_val = 0;
  filter_val = fkernel0.x;
  sum.x += filter_val * (int)((l.x >> 0) & 0xff);
  sum.y += filter_val * (int)((l.x >> 8) & 0xff);
  sum.z += filter_val * (int)((l.x >> 16) & 0xff);
  sum.w += filter_val * (int)((l.x >> 24) & 0xff);

  filter_val = fkernel0.y;
  sum.x += filter_val * (int)((l.x >> 8) & 0xff);
  sum.y += filter_val * (int)((l.x >> 16) & 0xff);
  sum.z += filter_val * (int)((l.x >> 24) & 0xff);
  sum.w += filter_val * (int)((l.y >> 0) & 0xff);

  filter_val = fkernel0.z;
  sum.x += filter_val * (int)((l.x >> 16) & 0xff);
  sum.y += filter_val * (int)((l.x >> 24) & 0xff);
  sum.z += filter_val * (int)((l.y >> 0) & 0xff);
  sum.w += filter_val * (int)((l.y >> 8) & 0xff);

  filter_val = fkernel0.w;
  sum.x += filter_val * (int)((l.x >> 24) & 0xff);
  sum.y += filter_val * (int)((l.y >> 0) & 0xff);
  sum.z += filter_val * (int)((l.y >> 8) & 0xff);
  sum.w += filter_val * (int)((l.y >> 16) & 0xff);

  filter_val = fkernel1.x;
  sum.x += filter_val * (int)((l.y >> 0) & 0xff);
  sum.y += filter_val * (int)((l.y >> 8) & 0xff);
  sum.z += filter_val * (int)((l.y >> 16) & 0xff);
  sum.w += filter_val * (int)((l.y >> 24) & 0xff);

  filter_val = fkernel1.y;
  sum.x += filter_val * (int)((l.y >> 8) & 0xff);
  sum.y += filter_val * (int)((l.y >> 16) & 0xff);
  sum.z += filter_val * (int)((l.y >> 24) & 0xff);
  sum.w += filter_val * (int)((l.z >> 0) & 0xff);

  filter_val = fkernel1.z;
  sum.x += filter_val * (int)((l.y >> 16) & 0xff);
  sum.y += filter_val * (int)((l.y >> 24) & 0xff);
  sum.z += filter_val * (int)((l.z >> 0) & 0xff);
  sum.w += filter_val * (int)((l.z >> 8) & 0xff);

  filter_val = fkernel1.w;
  sum.x += filter_val * (int)((l.y >> 24) & 0xff);
  sum.y += filter_val * (int)((l.z >> 0) & 0xff);
  sum.z += filter_val * (int)((l.z >> 8) & 0xff);
  sum.w += filter_val * (int)((l.z >> 16) & 0xff);

  sum.x = (sum.x + FilterLineAdd8bit) >> FilterLineShift;
  sum.y = (sum.y + FilterLineAdd8bit) >> FilterLineShift;
  sum.z = (sum.z + FilterLineAdd8bit) >> FilterLineShift;
  sum.w = (sum.w + FilterLineAdd8bit) >> FilterLineShift;
  return sum;
}

int4 filter_line_hbd(RWByteAddressBuffer ref, int offset, int4 fkernel0, int4 fkernel1) {
  const int shift = 8 * (offset & 2);
  offset &= ~3;
  uint4 l0 = ref.Load4(offset);
  uint2 l1 = ref.Load2(offset + 16);

  l0.x = (l0.x >> shift) | ((l0.y << (24 - shift)) << 8);
  l0.y = (l0.y >> shift) | ((l0.z << (24 - shift)) << 8);
  l0.z = (l0.z >> shift) | ((l0.w << (24 - shift)) << 8);
  l0.w = (l0.w >> shift) | ((l1.x << (24 - shift)) << 8);
  l1.x = (l1.x >> shift) | ((l1.y << (24 - shift)) << 8);
  l1.y = l1.y >> shift;
  int4 sum = {0, 0, 0, 0};

  sum.x += fkernel0.x * (int)((l0.x >> 0) & 0xffff);
  sum.y += fkernel0.x * (int)((l0.x >> 16) & 0xffff);
  sum.z += fkernel0.x * (int)((l0.y >> 0) & 0xffff);
  sum.w += fkernel0.x * (int)((l0.y >> 16) & 0xffff);

  sum.x += fkernel0.y * (int)((l0.x >> 16) & 0xffff);
  sum.y += fkernel0.y * (int)((l0.y >> 0) & 0xffff);
  sum.z += fkernel0.y * (int)((l0.y >> 16) & 0xffff);
  sum.w += fkernel0.y * (int)((l0.z >> 0) & 0xffff);

  sum.x += fkernel0.z * (int)((l0.y >> 0) & 0xffff);
  sum.y += fkernel0.z * (int)((l0.y >> 16) & 0xffff);
  sum.z += fkernel0.z * (int)((l0.z >> 0) & 0xffff);
  sum.w += fkernel0.z * (int)((l0.z >> 16) & 0xffff);

  sum.x += fkernel0.w * (int)((l0.y >> 16) & 0xffff);
  sum.y += fkernel0.w * (int)((l0.z >> 0) & 0xffff);
  sum.z += fkernel0.w * (int)((l0.z >> 16) & 0xffff);
  sum.w += fkernel0.w * (int)((l0.w >> 0) & 0xffff);

  sum.x += fkernel1.x * (int)((l0.z >> 0) & 0xffff);
  sum.y += fkernel1.x * (int)((l0.z >> 16) & 0xffff);
  sum.z += fkernel1.x * (int)((l0.w >> 0) & 0xffff);
  sum.w += fkernel1.x * (int)((l0.w >> 16) & 0xffff);

  sum.x += fkernel1.y * (int)((l0.z >> 16) & 0xffff);
  sum.y += fkernel1.y * (int)((l0.w >> 0) & 0xffff);
  sum.z += fkernel1.y * (int)((l0.w >> 16) & 0xffff);
  sum.w += fkernel1.y * (int)((l1.x >> 0) & 0xffff);

  sum.x += fkernel1.z * (int)((l0.w >> 0) & 0xffff);
  sum.y += fkernel1.z * (int)((l0.w >> 16) & 0xffff);
  sum.z += fkernel1.z * (int)((l1.x >> 0) & 0xffff);
  sum.w += fkernel1.z * (int)((l1.x >> 16) & 0xffff);

  sum.x += fkernel1.w * (int)((l0.w >> 16) & 0xffff);
  sum.y += fkernel1.w * (int)((l1.x >> 0) & 0xffff);
  sum.z += fkernel1.w * (int)((l1.x >> 16) & 0xffff);
  sum.w += fkernel1.w * (int)((l1.y >> 0) & 0xffff);

  sum.x = (sum.x + FilterLineAdd10bit) >> FilterLineShift;
  sum.y = (sum.y + FilterLineAdd10bit) >> FilterLineShift;
  sum.z = (sum.z + FilterLineAdd10bit) >> FilterLineShift;
  sum.w = (sum.w + FilterLineAdd10bit) >> FilterLineShift;
  return sum;
}

int2 filter_line2(RWByteAddressBuffer ref, int offset, int4 fkernel0, int4 fkernel1) {
  uint4 l = ref.Load4(offset & (~3));
  uint shift = (offset & 3) * 8;
  l.x = (l.x >> shift) | ((l.y << (24 - shift)) << 8);
  l.y = (l.y >> shift) | ((l.z << (24 - shift)) << 8);
  l.z = (l.z >> shift);

  int2 sum = {0, 0};

  sum.x += fkernel0.x * (int)((l.x >> 0) & 0xff);
  sum.y += fkernel0.x * (int)((l.x >> 8) & 0xff);

  sum.x += fkernel0.y * (int)((l.x >> 8) & 0xff);
  sum.y += fkernel0.y * (int)((l.x >> 16) & 0xff);

  sum.x += fkernel0.z * (int)((l.x >> 16) & 0xff);
  sum.y += fkernel0.z * (int)((l.x >> 24) & 0xff);

  sum.x += fkernel0.w * (int)((l.x >> 24) & 0xff);
  sum.y += fkernel0.w * (int)((l.y >> 0) & 0xff);

  sum.x += fkernel1.x * (int)((l.y >> 0) & 0xff);
  sum.y += fkernel1.x * (int)((l.y >> 8) & 0xff);

  sum.x += fkernel1.y * (int)((l.y >> 8) & 0xff);
  sum.y += fkernel1.y * (int)((l.y >> 16) & 0xff);

  sum.x += fkernel1.z * (int)((l.y >> 16) & 0xff);
  sum.y += fkernel1.z * (int)((l.y >> 24) & 0xff);

  sum.x += fkernel1.w * (int)((l.y >> 24) & 0xff);
  sum.y += fkernel1.w * (int)((l.z >> 0) & 0xff);

  sum.x = (sum.x + FilterLineAdd8bit) >> FilterLineShift;
  sum.y = (sum.y + FilterLineAdd8bit) >> FilterLineShift;
  return sum;
}

int2 filter_line2_hbd(RWByteAddressBuffer ref, int offset, int4 fkernel0, int4 fkernel1) {
  const int shift = 8 * (offset & 2);
  offset &= ~3;
  uint4 l0 = ref.Load4(offset);
  uint l1 = ref.Load(offset + 16);

  l0.x = (l0.x >> shift) | ((l0.y << (24 - shift)) << 8);
  l0.y = (l0.y >> shift) | ((l0.z << (24 - shift)) << 8);
  l0.z = (l0.z >> shift) | ((l0.w << (24 - shift)) << 8);
  l0.w = (l0.w >> shift) | ((l1.x << (24 - shift)) << 8);
  l1.x = l1.x >> shift;
  int2 sum = {0, 0};

  sum.x += fkernel0.x * (int)((l0.x >> 0) & 0xffff);
  sum.y += fkernel0.x * (int)((l0.x >> 16) & 0xffff);
  sum.x += fkernel0.y * (int)((l0.x >> 16) & 0xffff);
  sum.y += fkernel0.y * (int)((l0.y >> 0) & 0xffff);
  sum.x += fkernel0.z * (int)((l0.y >> 0) & 0xffff);
  sum.y += fkernel0.z * (int)((l0.y >> 16) & 0xffff);
  sum.x += fkernel0.w * (int)((l0.y >> 16) & 0xffff);
  sum.y += fkernel0.w * (int)((l0.z >> 0) & 0xffff);
  sum.x += fkernel1.x * (int)((l0.z >> 0) & 0xffff);
  sum.y += fkernel1.x * (int)((l0.z >> 16) & 0xffff);
  sum.x += fkernel1.y * (int)((l0.z >> 16) & 0xffff);
  sum.y += fkernel1.y * (int)((l0.w >> 0) & 0xffff);
  sum.x += fkernel1.z * (int)((l0.w >> 0) & 0xffff);
  sum.y += fkernel1.z * (int)((l0.w >> 16) & 0xffff);
  sum.x += fkernel1.w * (int)((l0.w >> 16) & 0xffff);
  sum.y += fkernel1.w * (int)((l1.x >> 0) & 0xffff);

  sum.x = (sum.x + FilterLineAdd10bit) >> FilterLineShift;
  sum.y = (sum.y + FilterLineAdd10bit) >> FilterLineShift;
  return sum;
}

// The position val (1/16 sample) times the scale sf (1/16384), plus the offset of the scale, rounded to 1/1024
// sample away from zero: scaled_x() and scaled_y() of av1/common/scale.c, which need 64 bits for the product.
// HLSL 5.0 has no 64-bit integers, and double precision, which this function used before, is not executed by
// every GPU the decoder runs on (the console returned 0 here: docs/gpu-av1-decoder.md, "Global motion"). So
// the product is split: sf = (sf >> 8) * 256 + (sf & 255), and val * sf + off = 256 * hi + lo with both parts
// well inside 32 bits (|val| < 2^21, sf <= 2^15). testing/warp_check compares it with libaom.
int scale_value(int val, int sf) {
  const int off = (sf - (1 << REF_SCALE_SHIFT)) * (1 << (SUBPEL_BITS - 1));
  const int hi = val * (sf >> 8);
  const int lo = val * (sf & 255) + off;
  const int neg = (hi + (lo >> 8)) < 0;
  return neg ? hi - ((128 - lo) >> 8) : hi + ((lo + 128) >> 8);
}

int4 filter_line_warp(RWByteAddressBuffer ref, int offset, ByteAddressBuffer filter, int sx, int alpha) {
  uint4 l = ref.Load4(offset & (~3));
  uint shift = (offset & 3) * 8;
  l.x = (l.x >> shift) | ((l.y << (24 - shift)) << 8);
  l.y = (l.y >> shift) | ((l.z << (24 - shift)) << 8);
  l.z = (l.z >> shift) | ((l.w << (24 - shift)) << 8);
  int4 sum = int4(WarpHorizSumAdd, WarpHorizSumAdd, WarpHorizSumAdd, WarpHorizSumAdd);

  int filter_offset = WarpFilterSize * (((sx + WarpFiltRoundAdd) >> WarpFiltRoundBits) + WarpFiltOffset);
  uint4 f0 = filter.Load4(filter_offset);
  uint4 f1 = filter.Load4(filter_offset + 16);

  sum.x += f0.x * (int)((l.x >> 0) & 0xff);
  sum.x += f0.y * (int)((l.x >> 8) & 0xff);
  sum.x += f0.z * (int)((l.x >> 16) & 0xff);
  sum.x += f0.w * (int)((l.x >> 24) & 0xff);
  sum.x += f1.x * (int)((l.y >> 0) & 0xff);
  sum.x += f1.y * (int)((l.y >> 8) & 0xff);
  sum.x += f1.z * (int)((l.y >> 16) & 0xff);
  sum.x += f1.w * (int)((l.y >> 24) & 0xff);

  sx += alpha;
  filter_offset = WarpFilterSize * (((sx + WarpFiltRoundAdd) >> WarpFiltRoundBits) + WarpFiltOffset);
  f0 = filter.Load4(filter_offset);
  f1 = filter.Load4(filter_offset + 16);

  sum.y += f0.x * (int)((l.x >> 8) & 0xff);
  sum.y += f0.y * (int)((l.x >> 16) & 0xff);
  sum.y += f0.z * (int)((l.x >> 24) & 0xff);
  sum.y += f0.w * (int)((l.y >> 0) & 0xff);
  sum.y += f1.x * (int)((l.y >> 8) & 0xff);
  sum.y += f1.y * (int)((l.y >> 16) & 0xff);
  sum.y += f1.z * (int)((l.y >> 24) & 0xff);
  sum.y += f1.w * (int)((l.z >> 0) & 0xff);

  sx += alpha;
  filter_offset = WarpFilterSize * (((sx + WarpFiltRoundAdd) >> WarpFiltRoundBits) + WarpFiltOffset);
  f0 = filter.Load4(filter_offset);
  f1 = filter.Load4(filter_offset + 16);

  sum.z += f0.x * (int)((l.x >> 16) & 0xff);
  sum.z += f0.y * (int)((l.x >> 24) & 0xff);
  sum.z += f0.z * (int)((l.y >> 0) & 0xff);
  sum.z += f0.w * (int)((l.y >> 8) & 0xff);
  sum.z += f1.x * (int)((l.y >> 16) & 0xff);
  sum.z += f1.y * (int)((l.y >> 24) & 0xff);
  sum.z += f1.z * (int)((l.z >> 0) & 0xff);
  sum.z += f1.w * (int)((l.z >> 8) & 0xff);

  sx += alpha;
  filter_offset = WarpFilterSize * (((sx + WarpFiltRoundAdd) >> WarpFiltRoundBits) + WarpFiltOffset);
  f0 = filter.Load4(filter_offset);
  f1 = filter.Load4(filter_offset + 16);

  sum.w += f0.x * (int)((l.x >> 24) & 0xff);
  sum.w += f0.y * (int)((l.y >> 0) & 0xff);
  sum.w += f0.z * (int)((l.y >> 8) & 0xff);
  sum.w += f0.w * (int)((l.y >> 16) & 0xff);
  sum.w += f1.x * (int)((l.y >> 24) & 0xff);
  sum.w += f1.y * (int)((l.z >> 0) & 0xff);
  sum.w += f1.z * (int)((l.z >> 8) & 0xff);
  sum.w += f1.w * (int)((l.z >> 16) & 0xff);

  sum.x = sum.x >> WarpHorizBits;
  sum.y = sum.y >> WarpHorizBits;
  sum.z = sum.z >> WarpHorizBits;
  sum.w = sum.w >> WarpHorizBits;
  return sum;
}

int4 filter_line_warp_hbd(RWByteAddressBuffer ref, int offset, ByteAddressBuffer filter, int sx, int alpha) {
  uint shift = (offset & 3) * 8;
  offset &= ~3;
  uint4 l0 = ref.Load4(offset);
  uint2 l4 = ref.Load2(offset + 16);
  l0.x = (l0.x >> shift) | ((l0.y << (24 - shift)) << 8);
  l0.y = (l0.y >> shift) | ((l0.z << (24 - shift)) << 8);
  l0.z = (l0.z >> shift) | ((l0.w << (24 - shift)) << 8);
  l0.w = (l0.w >> shift) | ((l4.x << (24 - shift)) << 8);
  l4.x = (l4.x >> shift) | ((l4.y << (24 - shift)) << 8);
  l4.y >>= shift;

  int4 sum = int4(WarpHorizSumAdd10, WarpHorizSumAdd10, WarpHorizSumAdd10, WarpHorizSumAdd10);

  int filter_offset = WarpFilterSize * (((sx + WarpFiltRoundAdd) >> WarpFiltRoundBits) + WarpFiltOffset);
  uint4 f0 = filter.Load4(filter_offset);
  uint4 f1 = filter.Load4(filter_offset + 16);

  sum.x += f0.x * (int)((l0.x >> 0) & PixelMax10);
  sum.x += f0.y * (int)((l0.x >> 16) & PixelMax10);
  sum.x += f0.z * (int)((l0.y >> 0) & PixelMax10);
  sum.x += f0.w * (int)((l0.y >> 16) & PixelMax10);
  sum.x += f1.x * (int)((l0.z >> 0) & PixelMax10);
  sum.x += f1.y * (int)((l0.z >> 16) & PixelMax10);
  sum.x += f1.z * (int)((l0.w >> 0) & PixelMax10);
  sum.x += f1.w * (int)((l0.w >> 16) & PixelMax10);

  sx += alpha;
  filter_offset = WarpFilterSize * (((sx + WarpFiltRoundAdd) >> WarpFiltRoundBits) + WarpFiltOffset);
  f0 = filter.Load4(filter_offset);
  f1 = filter.Load4(filter_offset + 16);

  sum.y += f0.x * (int)((l0.x >> 16) & PixelMax10);
  sum.y += f0.y * (int)((l0.y >> 0) & PixelMax10);
  sum.y += f0.z * (int)((l0.y >> 16) & PixelMax10);
  sum.y += f0.w * (int)((l0.z >> 0) & PixelMax10);
  sum.y += f1.x * (int)((l0.z >> 16) & PixelMax10);
  sum.y += f1.y * (int)((l0.w >> 0) & PixelMax10);
  sum.y += f1.z * (int)((l0.w >> 16) & PixelMax10);
  sum.y += f1.w * (int)((l4.x >> 0) & PixelMax10);

  sx += alpha;
  filter_offset = WarpFilterSize * (((sx + WarpFiltRoundAdd) >> WarpFiltRoundBits) + WarpFiltOffset);
  f0 = filter.Load4(filter_offset);
  f1 = filter.Load4(filter_offset + 16);

  sum.z += f0.x * (int)((l0.y >> 0) & PixelMax10);
  sum.z += f0.y * (int)((l0.y >> 16) & PixelMax10);
  sum.z += f0.z * (int)((l0.z >> 0) & PixelMax10);
  sum.z += f0.w * (int)((l0.z >> 16) & PixelMax10);
  sum.z += f1.x * (int)((l0.w >> 0) & PixelMax10);
  sum.z += f1.y * (int)((l0.w >> 16) & PixelMax10);
  sum.z += f1.z * (int)((l4.x >> 0) & PixelMax10);
  sum.z += f1.w * (int)((l4.x >> 16) & PixelMax10);

  sx += alpha;
  filter_offset = WarpFilterSize * (((sx + WarpFiltRoundAdd) >> WarpFiltRoundBits) + WarpFiltOffset);
  f0 = filter.Load4(filter_offset);
  f1 = filter.Load4(filter_offset + 16);

  sum.w += f0.x * (int)((l0.y >> 16) & PixelMax10);
  sum.w += f0.y * (int)((l0.z >> 0) & PixelMax10);
  sum.w += f0.z * (int)((l0.z >> 16) & PixelMax10);
  sum.w += f0.w * (int)((l0.w >> 0) & PixelMax10);
  sum.w += f1.x * (int)((l0.w >> 16) & PixelMax10);
  sum.w += f1.y * (int)((l4.x >> 0) & PixelMax10);
  sum.w += f1.z * (int)((l4.x >> 16) & PixelMax10);
  sum.w += f1.w * (int)((l4.y >> 0) & PixelMax10);

  sum.x = sum.x >> WarpHorizBits;
  sum.y = sum.y >> WarpHorizBits;
  sum.z = sum.z >> WarpHorizBits;
  sum.w = sum.w >> WarpHorizBits;
  return sum;
}
