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

cbuffer IntraDataCommon : register(b0) {
  int4 cb_planes[3];
  int4 cb_flags;
  int4 cb_filter[5][8][2];
  int4 cb_mode_params_lut[16][7];
  int4 cb_sm_weight_arrays[128];
};

cbuffer PSSLIntraSRT : register(b1) {
  int4 cb_counts0;
  int4 cb_counts1;
  uint cb_wi_count;
  int cb_pass_offset;
};

ByteAddressBuffer pred_blocks : register(t0);
ByteAddressBuffer residuals : register(t1);
ByteAddressBuffer wedge_mask : register(t2);
RWByteAddressBuffer dst_frame : register(u0);

#define Dir1 1
#define Dir2 2
#define Dir3 3
#define SmoothV 4
#define SmoothH 8

#define MODE_PAETH 11
#define MODE_INTRA_BC 12
#define MODE_FILTER 13
#define MODE_DC 14
#define MODE_CFL 15

#define NeedAboveShift 4
#define NeedRightShift 5
#define NeedLeftShift 6
#define NeedBotShift 7
#define NeedAboveLeftShift 8
#define FilterAboveLeftFlag 0x200
#define NeedAboveLeftLUT 0x08ff
// intra bc:
#define SubpelBits 4
#define FilterLineShift 3
#define OutputShift 11
#define OffsetBits 19
#define SumAddHor (1 << 14)
#define SumAddVert ((1 << OffsetBits) + (1 << (OutputShift - 1)))
#define OutputSub ((1 << (OffsetBits - OutputShift)) + (1 << (OffsetBits - OutputShift - 1)))

groupshared int above[64 * 20];
groupshared int left[64 * 20];

int compute_bc(int p0, int p1, int p2, int p3, int fh, int fv) {
  int l0 = ((1 << 14) + p0 * fh + p1 * (128 - fh) + (1 << (FilterLineShift - 1))) >> FilterLineShift;
  int l1 = ((1 << 14) + p2 * fh + p3 * (128 - fh) + (1 << (FilterLineShift - 1))) >> FilterLineShift;
  int output = SumAddVert + l0 * fv + l1 * (128 - fv);
  return clamp((output >> OutputShift) - OutputSub, 0, 1023);
}

uint compute_cfl_pixel(int dc, int value) {
  value = (value < 0) ? -((-value + (1 << 5)) >> 6) : ((value + (1 << 5)) >> 6);
  return clamp(dc + value, 0, 1023);
}

int intra_inter_blend(int mask, int intra, int inter) {
  return clamp((intra * mask + inter * (64 - mask) + 32) >> 6, 0, 1023);
}
#define WG_SIZE 256
[numthreads(WG_SIZE, 1, 1)] void main(uint3 thread
                                      : SV_DispatchThreadID) {
  int4 counts0 = cb_counts0;
  int4 counts1 = cb_counts1;
  int bsize_log = 10;
  int offset = cb_pass_offset;
  int threadx = thread.x;
  uint3 block = uint3(0, 0, 0);
  int wi_count = 0;
  int wi = 1024;
  if (thread.x < cb_wi_count) {
    if (threadx >= (counts0.x << 10)) {
      offset += cb_counts0.x;
      threadx -= counts0.x << 10;
      bsize_log = 9;
      if (threadx >= (counts0.y << 9)) {
        offset += cb_counts0.y;
        threadx -= counts0.y << 9;
        bsize_log = 8;
        if (threadx >= (counts0.z << 8)) {
          offset += cb_counts0.z;
          threadx -= counts0.z << 8;
          bsize_log = 7;
          if (threadx >= (counts0.w << 7)) {
            offset += cb_counts0.w;
            threadx -= counts0.w << 7;
            bsize_log = 6;
            if (threadx >= (counts1.x << 6)) {
              offset += cb_counts1.x;
              threadx -= counts1.x << 6;
              bsize_log = 5;
              if (threadx >= (counts1.y << 5)) {
                offset += cb_counts1.y;
                threadx -= counts1.y << 5;
                bsize_log = 4;
                if (threadx >= (counts1.z << 4)) {
                  offset += cb_counts1.z;
                  threadx -= counts1.z << 4;
                  bsize_log = 3;
                  if (threadx >= (counts1.w << 3)) {
                    offset += cb_counts1.w;
                    threadx -= counts1.w << 3;
                    bsize_log = 2;
                  }
                }
              }
            }
          }
        }
      }
    }

    int block_index = offset + (threadx >> bsize_log);
    block = pred_blocks.Load3(block_index << 4);
    wi_count = min(WG_SIZE, 1 << bsize_log);
    wi = thread.x & (wi_count - 1);
  }
  const int bw_log = block.y & 7;  // 0..4
  const int bw = 4 << bw_log;
  const int bh = 1 << (bsize_log - bw_log);

  int bx = (block.x & 0xffff) << 2;
  int by = (block.x >> 16) << 2;

  const int plane = (block.y >> 3) & 3;
  const int above_available = ((block.y >> 10) & 63) << 1;
  const int left_available = ((block.y >> 16) & 63) << 2;

  const int mode = (block.y >> 6) & 15;

  const int mode_angle = (block.y >> 28) & 7;
  int4 params = cb_mode_params_lut[mode][mode_angle];

  //    block.y bits:
  //    0    3        bw_log
  //    3    2        plane
  //    5    1        non skip
  //    6    4        mode
  //        all mods except intra_bc:
  //    10    6        above_available
  //    16    6        left_available
  //            dir mode params:
  //    22    2        upsample
  //    24    2        edge_filter above
  //    26    2        edge_filter left
  //    28    3        mode_angle
  //    31    1        inter_intra?
  //            CFL:
  //    22    4        alpha
  //
  //    block.z
  //        intra_bc - mv
  //        inter-intra    - coef. table indexes;
  //        filter - bh_log | filter_mode;
  //    block.w - reserved; (prob. used for sorting);

  const int stride = cb_planes[plane].x;
  const int corner = cb_planes[plane].y + ((bx << 1) - 4) + (by - 1) * stride;

  const int loc_base = (((thread.x & (WG_SIZE - 1)) - wi) >> 2) * 20 + 2;
  int above_count = ((params.z >> NeedAboveShift) & 1) * bw + ((params.z >> NeedRightShift) & 1) * bh;

  if (wi < (above_count >> 1) || wi == 0) {
    int addr = corner + 4 + 4 * min(wi, above_available - 1);
    uint pixels =
        above_available ? dst_frame.Load(addr) : left_available ? dst_frame.Load(corner + stride) : 0x01ff0000;
    if (wi >= above_available) pixels = (pixels & 0xffff0000) | (pixels >> 16);
    above[loc_base + (wi << 1) + 0] = (pixels >> 0) & 1023;
    above[loc_base + (wi << 1) + 1] = (pixels >> 16) & 1023;
  }

  // Left:
  GroupMemoryBarrierWithGroupSync();

  int left_count = ((params.z >> NeedLeftShift) & 1) * bh + ((params.z >> NeedBotShift) & 1) * bw;

  if (wi < left_count || wi == 0) {
    const int addr = corner + (min(wi, left_available - 1) + 1) * stride;
    left[loc_base + wi] = left_available ? (dst_frame.Load(addr) >> 16) : above_available ? above[loc_base] : 513;
  }
  if (wi < (left_count - wi_count)) {
    const int addr = corner + (min(wi + wi_count, left_available - 1) + 1) * stride;
    left[loc_base + wi + wi_count] =
        left_available ? (dst_frame.Load(addr) >> 16) : above_available ? above[loc_base] : 513;
  }

  GroupMemoryBarrierWithGroupSync();

  const int need_aboveleft = (params.z >> NeedAboveLeftShift) & 1;
  if (wi == 0 && need_aboveleft) {
    uint t = above[loc_base];
    uint l = left[loc_base];
    uint topleft = (left_available && above_available) ? (dst_frame.Load(corner) >> 16)
                                                       : left_available ? l : above_available ? t : 512;

    if ((params.z & FilterAboveLeftFlag) && (bw + bh >= 24) && cb_flags.x)
      topleft = ((l + t) * 5 + topleft * 6 + 8) >> 4;

    above[loc_base - 1] = topleft;
    left[loc_base - 1] = topleft;
    above[loc_base - 2] = topleft;
    left[loc_base - 2] = topleft;
  }

  GroupMemoryBarrierWithGroupSync();

  int dir_mode = mode < MODE_DC;
  int upsample_above = (block.y >> 22) & dir_mode;
  int upsample_left = (block.y >> 23) & dir_mode;

  int filter_count = 0;
  if (above_available && ((params.z >> NeedAboveShift) & 1)) {
    filter_count = min(above_available << 2, bw) + ((params.z >> NeedRightShift) & 1) * bh + need_aboveleft - 1;
  }
  int edge_filter = (block.y >> 24) & ((wi < filter_count && dir_mode) * 3);

  int sum0 = 8;
  int sum1 = 8;
  const int wi1 = wi + wi_count;
  if (edge_filter) {
    edge_filter <<= 2;
    const int base = loc_base - need_aboveleft;
    const int last = filter_count;
    //{ 0, 4, 8, 4, 0 }, { 0, 5, 6, 5, 0 }, { 2, 4, 4, 4, 2 }
    sum0 += above[base + max(wi - 1, 0)] * ((0x2000 >> edge_filter) & 15);
    sum0 += above[base + (wi + 0)] * ((0x4540 >> edge_filter) & 15);
    sum0 += above[base + (wi + 1)] * ((0x4680 >> edge_filter) & 15);
    sum0 += above[base + min(wi + 2, last)] * ((0x4540 >> edge_filter) & 15);
    sum0 += above[base + min(wi + 3, last)] * ((0x2000 >> edge_filter) & 15);
    if (wi1 < filter_count)  // rare, some mods for 4x4, 8x4, 4x8 blocks
    {
      sum1 += above[base + max(wi1 - 1, 0)] * ((0x2000 >> edge_filter) & 15);
      sum1 += above[base + (wi1 + 0)] * ((0x4540 >> edge_filter) & 15);
      sum1 += above[base + (wi1 + 1)] * ((0x4680 >> edge_filter) & 15);
      sum1 += above[base + min(wi1 + 2, last)] * ((0x4540 >> edge_filter) & 15);
      sum1 += above[base + min(wi1 + 3, last)] * ((0x2000 >> edge_filter) & 15);
    }
  }
  GroupMemoryBarrierWithGroupSync();
  if (edge_filter) {
    above[loc_base + wi] = sum0 >> 4;
    if (wi1 < filter_count) {
      above[loc_base + wi1] = sum1 >> 4;
    }
  }

  if (left_available && ((params.z >> NeedLeftShift) & 1)) {
    filter_count = min(left_available, bh) + ((params.z >> NeedBotShift) & 1) * bw + need_aboveleft - 1;
  }
  edge_filter = (block.y >> 26) & ((wi < filter_count && dir_mode) * 3);
  if (edge_filter) {
    sum0 = 8;
    sum1 = 8;
    edge_filter *= 4;
    const int base = loc_base - need_aboveleft;
    const int last = filter_count;
    sum0 += left[base + max(wi - 1, 0)] * ((0x2000 >> edge_filter) & 15);
    sum0 += left[base + (wi + 0)] * ((0x4540 >> edge_filter) & 15);
    sum0 += left[base + (wi + 1)] * ((0x4680 >> edge_filter) & 15);
    sum0 += left[base + min(wi + 2, last)] * ((0x4540 >> edge_filter) & 15);
    sum0 += left[base + min(wi + 3, last)] * ((0x2000 >> edge_filter) & 15);
    if (wi1 < filter_count) {
      sum1 += left[base + max(wi1 - 1, 0)] * ((0x2000 >> edge_filter) & 15);
      sum1 += left[base + (wi1 + 0)] * ((0x4540 >> edge_filter) & 15);
      sum1 += left[base + (wi1 + 1)] * ((0x4680 >> edge_filter) & 15);
      sum1 += left[base + min(wi1 + 2, last)] * ((0x4540 >> edge_filter) & 15);
      sum1 += left[base + min(wi1 + 3, last)] * ((0x2000 >> edge_filter) & 15);
    }
  }
  GroupMemoryBarrierWithGroupSync();
  if (edge_filter) {
    left[loc_base + wi] = sum0 >> 4;
    if (wi1 < filter_count) {
      left[loc_base + wi1] = sum1 >> 4;
    }
  }

  GroupMemoryBarrierWithGroupSync();

  int p0 = 0, p1 = 0, p2 = 0, p3 = 0, p4 = 0;
  int do_upsample = upsample_above && wi < (above_count >> 1);
  if (do_upsample) {
    p0 = above[loc_base + wi * 2 - 2];
    p1 = above[loc_base + wi * 2 - 1];
    p2 = above[loc_base + wi * 2 + 0];
    p3 = above[loc_base + wi * 2 + 1];
    p4 = above[loc_base + min(wi * 2 + 2, above_count - 1)];
  }
  GroupMemoryBarrierWithGroupSync();
  if (do_upsample) {
    above[loc_base - 1 + wi * 4] = clamp((-p0 + 9 * p1 + 9 * p2 - p3 + 8) >> 4, 0, 1023);
    above[loc_base + 0 + wi * 4] = p2;
    above[loc_base + 1 + wi * 4] = clamp((-p1 + 9 * p2 + 9 * p3 - p4 + 8) >> 4, 0, 1023);
    above[loc_base + 2 + wi * 4] = p3;
  }

  do_upsample = upsample_left && wi < (left_count >> 1);
  if (do_upsample) {
    p0 = left[loc_base + wi * 2 - 2];
    p1 = left[loc_base + wi * 2 - 1];
    p2 = left[loc_base + wi * 2 + 0];
    p3 = left[loc_base + wi * 2 + 1];
    p4 = left[loc_base + min(wi * 2 + 2, left_count - 1)];
  }
  GroupMemoryBarrierWithGroupSync();
  if (do_upsample) {
    left[loc_base - 1 + wi * 4] = clamp((-p0 + 9 * p1 + 9 * p2 - p3 + 8) >> 4, 0, 1023);
    left[loc_base + 0 + wi * 4] = p2;
    left[loc_base + 1 + wi * 4] = clamp((-p1 + 9 * p2 + 9 * p3 - p4 + 8) >> 4, 0, 1023);
    left[loc_base + 2 + wi * 4] = p3;
  }
  GroupMemoryBarrierWithGroupSync();

  // int x0 = (wi & ((1 << bw_log) - 1)) << 2;
  // int y = wi >> bw_log;
  int x0 = (thread.x & ((1 << bw_log) - 1)) << 2;
  int y = (thread.x & ((1 << bsize_log) - 1)) >> bw_log;

  uint2 pixels = uint2(0, 0);
  if (mode == MODE_INTRA_BC) {
    int mv = (int)block.z;
    int mvx = bx + x0 + ((mv) >> (16 + SubpelBits));
    int mvy = by + y + ((mv << 16) >> (16 + SubpelBits));
    const int filt_h = 128 >> ((mv >> 19) & 1);
    const int filt_v = 128 >> ((mv >> 3) & 1);
    int addr = cb_planes[plane].y + (mvx << 1) + mvy * stride;
    uint3 ref0, ref1 = 0;

    const uint shift = (addr & 2) << 3;
    addr &= ~3;
    ref0 = dst_frame.Load3(addr);
    ref0.x = (ref0.x >> shift) | ((ref0.y << (24 - shift)) << 8);
    ref0.y = (ref0.y >> shift) | ((ref0.z << (24 - shift)) << 8);
    ref0.z = ref0.z >> shift;

    if (filt_v != 128) {
      ref1 = dst_frame.Load3(addr + stride);
      ref1.x = (ref1.x >> shift) | ((ref1.y << (24 - shift)) << 8);
      ref1.y = (ref1.y >> shift) | ((ref1.z << (24 - shift)) << 8);
      ref1.z = ref1.z >> shift;
    }

    pixels.x = (compute_bc((ref0.x >> 0) & 1023, (ref0.x >> 16) & 1023, (ref1.x >> 0) & 1023, (ref1.x >> 16) & 1023,
                           filt_h, filt_v)
                << 0) |
               (compute_bc((ref0.x >> 16) & 1023, (ref0.y >> 0) & 1023, (ref1.x >> 8) & 1023, (ref1.y >> 0) & 1023,
                           filt_h, filt_v)
                << 16);
    pixels.y = (compute_bc((ref0.y >> 0) & 1023, (ref0.y >> 16) & 1023, (ref1.y >> 0) & 1023, (ref1.y >> 16) & 1023,
                           filt_h, filt_v)
                << 0) |
               (compute_bc((ref0.y >> 16) & 1023, (ref0.z >> 0) & 1023, (ref1.y >> 16) & 1023, (ref1.z >> 0) & 1023,
                           filt_h, filt_v)
                << 16);
  }

  uint dc = 0;
  if (mode >= MODE_DC) {
    uint count = 0;
    // todo: maybe some optization here?
    if (above_available) {
      for (int i = 0; i < bw; ++i) dc += above[loc_base + i];
      count += bw;
    }
    if (left_available) {
      for (int i = 0; i < bh; ++i) dc += left[loc_base + i];
      count += bh;
    }
    dc += count >> 1;
    dc = count ? dc / count : 512;

    pixels.x = dc | (dc << 16);
    pixels.y = pixels.x;
  }

  GroupMemoryBarrierWithGroupSync();

  int4 luma = 0;
  if (mode == MODE_CFL) {
    const int max_y = (block.z >> 16) - 2;
    const int y_stride = cb_planes[0].x;
    const int luma_y = min((by + y) << 1, max_y);

    // const int max_x = (block.z & 0xffff) - 4;
    // const int luma_x = (bx + x0) << 1;
    // int y_offset = cb_planes[0].y + min(luma_x, max_x) + luma_y * y_stride;
    const int max_x = ((block.z & 0xffff) << 1) - 8;
    const int luma_x = (bx + x0) << 2;
    int y_offset = cb_planes[0].y + min(luma_x, max_x) + luma_y * y_stride;

    uint4 luma0 = dst_frame.Load4(y_offset);
    uint4 luma1 = dst_frame.Load4(y_offset + y_stride);

    if (luma_x >= max_x) {
      luma0.zw = luma0.yy;
      luma1.zw = luma1.yy;
      if (luma_x > max_x) {
        luma0.x = luma0.y;
        luma1.x = luma1.y;
      }
    }

    luma.x = ((luma0.x >> 0) & 1023) + ((luma0.x >> 16) & 1023) + ((luma1.x >> 0) & 1023) + ((luma1.x >> 16) & 1023);
    luma.y = ((luma0.y >> 0) & 1023) + ((luma0.y >> 16) & 1023) + ((luma1.y >> 0) & 1023) + ((luma1.y >> 16) & 1023);
    luma.z = ((luma0.z >> 0) & 1023) + ((luma0.z >> 16) & 1023) + ((luma1.z >> 0) & 1023) + ((luma1.z >> 16) & 1023);
    luma.w = ((luma0.w >> 0) & 1023) + ((luma0.w >> 16) & 1023) + ((luma1.w >> 0) & 1023) + ((luma1.w >> 16) & 1023);
    luma <<= 1;
    above[loc_base + wi] = luma.x + luma.y + luma.z + luma.w;
  }

  GroupMemoryBarrierWithGroupSync();

  if (mode == MODE_CFL)  // reduce
  {
    int count = wi_count >> 2;
    // max cfl block  32x32    /    4(pixels) = 256 wi (wi_count);
    // count = 256wi / 4  = up to 64wi == wavefront size, => we can use GroupMemoryBarrier();
    int ofs = loc_base + wi * 4;
    if (wi < count) {
      int sum = above[ofs + 0] + above[ofs + 1] + above[ofs + 2] + above[ofs + 3];
      GroupMemoryBarrier();
      above[loc_base + wi] = sum;
      GroupMemoryBarrier();
    }
    ofs = loc_base + wi * 2;
    count >>= 1;
    while (wi < count) {
      int sum = above[ofs + 0] + above[ofs + 1];
      GroupMemoryBarrier();
      above[loc_base + wi] = sum;
      GroupMemoryBarrier();
      count >>= 1;
    }
  }

  GroupMemoryBarrierWithGroupSync();
  if (thread.x >= cb_wi_count) return;

  if (mode == MODE_CFL) {
    int avrg = (above[loc_base] + (2 << bsize_log)) >> (bsize_log + 2);
    int alpha = ((block.y >> 22) & 63) - 16;
    pixels.x = compute_cfl_pixel(dc, (luma.x - avrg) * alpha);
    pixels.x |= compute_cfl_pixel(dc, (luma.y - avrg) * alpha) << 16;
    pixels.y = compute_cfl_pixel(dc, (luma.z - avrg) * alpha);
    pixels.y |= compute_cfl_pixel(dc, (luma.w - avrg) * alpha) << 16;
  }

  if (mode < MODE_INTRA_BC) {
    const int frac_bits_x = 6 - upsample_above;
    const int frac_bits_y = 6 - upsample_left;
    const int min_base_x = -(1 << upsample_above);

    above_count <<= upsample_above;
    left_count <<= upsample_left;
    uint topleft = above[loc_base - 1];

    for (int i = 0; i < 4; ++i) {
      int x = i + x0;
      ////1:
      // int offset_x = (1 + y) * dx;
      // int base_x = offset_x >> frac_bits_x;
      //
      ////2:
      // int offset_x = (x << 6) - (y + 1) * dx;
      // const int base_x = offset_x >> frac_bits_x;
      //
      // int offset_y = (y << 6) - (x + 1) * dy;
      // const int base_y = offset_y >> frac_bits_y;
      //
      ////3:
      // int offset_y = (1 + x) * dy;
      // int base_y = offset_x >> frac_bits_x;
      //
      // int shift_y = ((offset_y << upsample_above) & 0x3F) >> 1;
      // int shift_x = ((offset_x << upsample_above) & 0x3F) >> 1;
      // Note:     for non dir modes:
      //        dx = 0; dy = 0;     upsample_above = 0;     upsample_left = 0;

      const int offset_x = (x << 6) + (y + 1) * params.x;
      const int offset_y = (y << 6) + (x + 1) * params.y;
      const int shift_x = ((offset_x << upsample_above) & 0x3F) >> 1;
      const int shift_y = ((offset_y << upsample_left) & 0x3F) >> 1;

      // Note: shift_x = 0 & shift_y = 0 if (dx == 0) & (dy == 0);
      // maybe this can be used to simplify weight calc;

      const int base_x = offset_x >> frac_bits_x;
      int l0 = left[loc_base + clamp(offset_y >> frac_bits_y, -2, left_count - 1)];
      int l1 = left[loc_base + clamp((offset_y >> frac_bits_y) + 1, -2, left_count - 1)];
      int t0 = above[loc_base + clamp(base_x, -2, above_count - 1)];
      int t1 = above[loc_base + clamp(base_x + 1, -2, above_count - 1)];

      // switch(mode)
      //{
      //    case (SMOOTH_V | SMOOTH_H):
      //        w_t0 = *(sm_weight_arrays + bh + y);
      //        w_l0 = *(sm_weight_arrays + bw + x);
      //
      //    case SMOOTH_V:
      //        w_t0 = *(sm_weight_arrays + bh + y);
      //
      //    case SMOOTH_H:
      //        w_l0 = *(sm_weight_arrays + bw + x);
      //
      //    case V:
      //    case DIR_1:
      //        w_t0 = 32 - shift_x;
      //
      //    case H:
      //    case DIR_3:
      //        w_l0 = 32 - shift_y;
      //
      //    case MODE_PAETH:
      //        int base = t0 + l0 - topleft;
      //        int diff_t = abs(t0 - base);
      //        int diff_l = abs(l0 - base);
      //        int diff_tl = abs(topleft - base);
      //        w_t0 = diff_t < diff_l && diff_t < diff_tl;
      //        w_l0 = diff_l < diff_t && diff_l < diff_tl;
      //};

      int mode_info = params.z & 15;
      if (mode_info == Dir2)  // eliminate dir2
        mode_info = base_x >= min_base_x ? Dir1 : Dir3;

      int w_t0 = (mode_info & SmoothV) ? cb_sm_weight_arrays[bh + y].x : (mode_info == Dir1) ? (32 - shift_x) : 0;
      int w_l0 = (mode_info & SmoothH) ? cb_sm_weight_arrays[bw + x].x : (mode_info == Dir3) ? (32 - shift_y) : 0;

      if (mode == MODE_PAETH) {
        int diff_t = topleft - l0;
        int diff_l = topleft - t0;
        int diff_tl = abs(diff_t + diff_l);
        diff_t = abs(diff_t);
        diff_l = abs(diff_l);
        w_l0 = diff_l <= diff_t && diff_l <= diff_tl;
        w_t0 = diff_t < diff_l && diff_t <= diff_tl;
        w_l0 <<= 5;
        w_t0 <<= 5;
      }

      int w_tl = (mode == MODE_PAETH) ? (32 - w_t0 - w_l0) : 0;
      int w_l1 = (mode_info == Dir3) ? shift_y : 0;
      int w_t1 = (mode_info == Dir1) ? shift_x : 0;
      int w_b = (mode_info & SmoothV) ? 0x100 - w_t0 : 0;
      int w_r = (mode_info & SmoothH) ? 0x100 - w_l0 : 0;

      int sum = w_tl * topleft + w_b * left[loc_base + bh - 1] + w_r * above[loc_base + bw - 1] + w_l0 * l0 +
                w_t0 * t0 + w_l1 * l1 + w_t1 * t1;

      sum += 1 << (params.w - 1);
      sum >>= params.w;
      int val = clamp(sum, 0, 1023) << ((i & 1) * 16);
      if (i & 2)
        pixels.y |= val;
      else
        pixels.x |= val;
    }
  }

  const int addr = corner + 4 + (x0 << 1) + (y + 1) * stride;
  if (block.y & 0x80000000)  // inter-intra
  {
    int wedge_addr = (block.z << 6) + x0 + y * bw;
    uint wedge = wedge_mask.Load(wedge_addr);
    uint2 inter = dst_frame.Load2(addr);

    pixels.x = intra_inter_blend((wedge >> 0) & 255, (pixels.x >> 0) & 1023, (inter.x >> 0) & 1023) |
               (intra_inter_blend((wedge >> 8) & 255, (pixels.x >> 16) & 1023, (inter.x >> 16) & 1023) << 16);
    pixels.y = intra_inter_blend((wedge >> 16) & 255, (pixels.y >> 0) & 1023, (inter.y >> 0) & 1023) |
               (intra_inter_blend((wedge >> 24) & 255, (pixels.y >> 16) & 1023, (inter.y >> 16) & 1023) << 16);
  }

  if (block.y & (1 << 5)) {
    const int res_stride = cb_planes[plane].z;
    const int res_addr = cb_planes[plane].w + ((bx + x0) << 1) + (by + y) * res_stride;
    int2 res = residuals.Load2(res_addr);
    pixels.x = clamp((int)((pixels.x >> 0) & 1023) + (int)((res.x << 16) >> 16), 0, 1023) |
               (clamp((int)((pixels.x >> 16) & 1023) + (int)(res.x >> 16), 0, 1023) << 16);
    pixels.y = clamp((int)((pixels.y >> 0) & 1023) + (int)((res.y << 16) >> 16), 0, 1023) |
               (clamp((int)((pixels.y >> 16) & 1023) + (int)(res.y >> 16), 0, 1023) << 16);
  }

  dst_frame.Store2(addr, pixels);
}
