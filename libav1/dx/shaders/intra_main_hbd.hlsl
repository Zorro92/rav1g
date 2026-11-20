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
#define MODE_DC 14
#define MODE_CFL 15

#define NeedAboveShift 4
#define NeedRightShift 5
#define NeedLeftShift 6
#define NeedBotShift 7
#define NeedAboveLeftShift 8
#define FilterAboveLeftFlag 0x200
// intra bc:
#define SubpelBits 4
#define FilterLineShift 3
#define OutputShift 11
#define OffsetBits 19
#define SumAddHor (1 << 14)
#define SumAddVert ((1 << OffsetBits) + (1 << (OutputShift - 1)))
#define OutputSub ((1 << (OffsetBits - OutputShift)) + (1 << (OffsetBits - OutputShift - 1)))

#define WG_SIZE 256
#define EDGE_ABOVE 0
#define EDGE_LEFT 1

// FXC can turn dynamic masks into bit-field instructions with run-time widths.
static const uint low_bits[11] = {0, 1, 3, 7, 15, 31, 63, 127, 255, 511, 1023};

// Each transform owns 20 words per four lanes on each edge, including room for the corner and
// upsampling. DC and CFL blocks use the same space for their partial sums.
groupshared int edges[2][(WG_SIZE / 4) * 20];

#ifndef INTRA_MAIN_LEGACY
int reduction_stride(int base, int wi, int wi_count, int terms) {
  if (terms < 32) return 1;
  const int lanes = (int)WaveGetLaneCount();
  if (lanes < 32 || wi >= lanes) return 1;

  // The first min(lanes, wi_count) threads can read all of the block's partials
  // at wave-width strides. Vote on their block and population before using a
  // wave sum, since a wave may mix blocks or hold only part of these threads.
  if (WaveActiveAllEqual(base) && WaveActiveCountBits(true) == (uint)min(lanes, wi_count)) return lanes;
  return 1;
}

int sum_partials(int edge, int base, int wi, int count, int step) {
  int sum = 0;
  [loop] for (int i = wi; i < count; i += step) sum += edges[edge][base + i];
  if (step > 1) sum = WaveActiveSum(sum);
  return sum;
}
#endif

int filter_edge(int edge, int base, int i, int last, int strength) {
  const int outer = (0x2000 >> (4 * strength)) & 15;
  const int inner = (0x4540 >> (4 * strength)) & 15;
  const int center = (0x4680 >> (4 * strength)) & 15;
  return (8 + outer * (edges[edge][base + max(i - 1, 0)] + edges[edge][base + min(i + 3, last)]) +
          inner * (edges[edge][base + i] + edges[edge][base + min(i + 2, last)]) +
          center * edges[edge][base + i + 1]) >> 4;
}

int upsample_edge(int p0, int p1, int p2, int p3) {
  return clamp((-p0 + 9 * (p1 + p2) - p3 + 8) >> 4, 0, 1023);
}

// In gen_pred_blocks.hlsl, an edge is upsampled only for a small enough block and an angle
// difference below 40, where its filter strength is zero. Each lane prepares its outputs
// from the raw edge here, then writes them back after every lane has finished reading.
int4 prepare_edge(int edge, int base, int wi, int wi_count, int count, int upsample,
                  int filter_count, int strength, int need_aboveleft) {
  int4 values = 0;
  if (upsample && wi < (count >> 1)) {
    const int i = wi * 2;
    const int p0 = edges[edge][base + i - 2];
    const int p1 = edges[edge][base + i - 1];
    const int p2 = edges[edge][base + i];
    const int p3 = edges[edge][base + i + 1];
    const int p4 = edges[edge][base + min(i + 2, count - 1)];
    values = int4(upsample_edge(p0, p1, p2, p3), p2, upsample_edge(p1, p2, p3, p4), p3);
  } else if (strength && wi < filter_count) {
    values.x = filter_edge(edge, base - need_aboveleft, wi, filter_count, strength);
    if (wi + wi_count < filter_count)
      values.y = filter_edge(edge, base - need_aboveleft, wi + wi_count, filter_count, strength);
  }
  return values;
}

void write_edge(int edge, int base, int wi, int wi_count, int count, int upsample,
                int filter_count, int strength, int4 values) {
  if (upsample && wi < (count >> 1)) {
    const int i = base + wi * 4;
    edges[edge][i - 1] = values.x;
    edges[edge][i] = values.y;
    edges[edge][i + 1] = values.z;
    edges[edge][i + 2] = values.w;
  } else if (strength && wi < filter_count) {
    edges[edge][base + wi] = values.x;
    if (wi + wi_count < filter_count) edges[edge][base + wi + wi_count] = values.y;
  }
}

uint load_above_pair(int corner, int wi, int available) {
  uint pair = dst_frame.Load(corner + 4 + 4 * min(wi, available - 1));
  if (wi >= available) pair = (pair & 0xffff0000) | (pair >> 16);
  return pair;
}

int load_left(int corner, int stride, int wi, int available) {
  return dst_frame.Load(corner + (min(wi, available - 1) + 1) * stride) >> 16;
}

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

int4 cfl_luma_row(uint limits, int bx, int by, int x0, int y) {
  int4 luma;
  const int max_y = (limits >> 16) - 2;
  const int y_stride = cb_planes[0].x;
  const int luma_y = min((by + y) << 1, max_y);

  const int max_x = ((limits & 0xffff) << 1) - 8;
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
  return luma;
}

[numthreads(WG_SIZE, 1, 1)] void main(uint3 thread
                                      : SV_DispatchThreadID) {
  int4 counts0 = cb_counts0;
  int4 counts1 = cb_counts1;
  int bsize_log = 10;
  int offset = cb_pass_offset;
  int threadx = thread.x;
  uint3 block = uint3(0, 0, 0);
  int block_lane = 0;
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

    const int block_index = threadx >> bsize_log;
    block = pred_blocks.Load3((offset + block_index) << 4);
    block_lane = threadx & low_bits[bsize_log];
    wi_count = min(WG_SIZE, 1 << bsize_log);
    wi = block_lane & (WG_SIZE - 1);
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
  const int need_above = (params.z >> NeedAboveShift) & 1;
  const int need_right = (params.z >> NeedRightShift) & 1;
  const int need_left = (params.z >> NeedLeftShift) & 1;
  const int need_bot = (params.z >> NeedBotShift) & 1;
  const int need_aboveleft = (params.z >> NeedAboveLeftShift) & 1;
  const int above_count = need_above * bw + need_right * bh;
  const int left_count = need_left * bh + need_bot * bw;
  const int y = block_lane >> bw_log;
  const int x0 = (block_lane & low_bits[bw_log]) << 2;

  const int mode_info = params.z & 15;
  const bool directional = mode_info >= Dir1 && mode_info <= Dir3;
  const int upsample_above = directional ? ((block.y >> 22) & 1) : 0;
  const int upsample_left = directional ? ((block.y >> 23) & 1) : 0;
  const int strength_above = directional ? ((block.y >> 24) & 3) : 0;
  const int strength_left = directional ? ((block.y >> 26) & 3) : 0;
  const int filter_above = above_available && need_above
                               ? min(above_available << 2, bw) + need_right * bh + need_aboveleft - 1
                               : 0;
  const int filter_left = left_available && need_left
                              ? min(left_available, bh) + need_bot * bw + need_aboveleft - 1
                              : 0;

  int4 luma = 0;
  // DC and CFL share the first two phases with edge preparation. Each active lane contributes
  // up to two above pixels and one left pixel to DC, and four subsampled luma pixels to CFL.
  const int dc_above = above_available ? bw >> 1 : 0;
  const int dc_left = left_available ? bh : 0;
  const int dc_terms = max(dc_above, dc_left);
  if (mode >= MODE_DC) {
    if (wi < dc_terms) {
      int sum = 0;
      if (wi < dc_above) {
        const uint pair = load_above_pair(corner, wi, above_available);
        sum = (pair & 1023) + ((pair >> 16) & 1023);
      }
      if (wi < dc_left) sum += load_left(corner, stride, wi, left_available);
      edges[EDGE_ABOVE][loc_base + wi] = sum;
    }
    if (mode == MODE_CFL) {
      luma = cfl_luma_row(block.z, bx, by, x0, y);
      edges[EDGE_LEFT][loc_base + wi] = luma.x + luma.y + luma.z + luma.w;
    }
  } else if (mode < MODE_INTRA_BC && thread.x < cb_wi_count) {
    int first_above = 0;
    int first_left = 0;
    if (wi < (above_count >> 1) || wi == 0) {
      uint pair = 511 | (511 << 16);
      if (above_available) {
        pair = load_above_pair(corner, wi, above_available);
      } else if (left_available) {
        const uint p = load_left(corner, stride, 0, left_available) & 1023;
        pair = p | (p << 16);
      }
      first_above = pair & 1023;
      edges[EDGE_ABOVE][loc_base + wi * 2] = first_above;
      edges[EDGE_ABOVE][loc_base + wi * 2 + 1] = (pair >> 16) & 1023;
    }
    if (wi < left_count || wi == 0) {
      first_left = 513;
      if (left_available) {
        first_left = load_left(corner, stride, wi, left_available);
      } else if (above_available) {
        // The missing column repeats the first above pixel. Load it directly to avoid a group sync.
        first_left = wi == 0 ? first_above : dst_frame.Load(corner + 4) & 1023;
      }
      edges[EDGE_LEFT][loc_base + wi] = first_left;
    }
    if (wi + wi_count < left_count) {
      int value = first_left;
      if (left_available) value = load_left(corner, stride, wi + wi_count, left_available);
      edges[EDGE_LEFT][loc_base + wi + wi_count] = value;
    }

    if (wi == 0 && need_aboveleft) {
      int topleft = left_available ? first_left : above_available ? first_above : 512;
      if (left_available && above_available) topleft = dst_frame.Load(corner) >> 16;
      if ((params.z & FilterAboveLeftFlag) && bw + bh >= 24 && cb_flags.x)
        topleft = ((first_left + first_above) * 5 + topleft * 6 + 8) >> 4;
      edges[EDGE_ABOVE][loc_base - 1] = topleft;
      edges[EDGE_ABOVE][loc_base - 2] = topleft;
      edges[EDGE_LEFT][loc_base - 1] = topleft;
      edges[EDGE_LEFT][loc_base - 2] = topleft;
    }
  }

  GroupMemoryBarrierWithGroupSync();

  int4 next_above = 0;
  int4 next_left = 0;
  if (directional) {
    next_above = prepare_edge(EDGE_ABOVE, loc_base, wi, wi_count, above_count, upsample_above,
                              filter_above, strength_above, need_aboveleft);
    next_left = prepare_edge(EDGE_LEFT, loc_base, wi, wi_count, left_count, upsample_left,
                             filter_left, strength_left, need_aboveleft);
#ifdef INTRA_MAIN_LEGACY
  } else if (mode >= MODE_DC && wi == 0) {
    const uint count = dc_above * 2 + dc_left;
    uint sum = count >> 1;
    [loop] for (int i = 0; i < dc_terms; ++i) sum += edges[EDGE_ABOVE][loc_base + i];
    edges[EDGE_ABOVE][loc_base - 1] = count ? sum / count : 512;

    if (mode == MODE_CFL) {
      int total = 0;
      [loop] for (int i = 0; i < wi_count; ++i) total += edges[EDGE_LEFT][loc_base + i];
      edges[EDGE_LEFT][loc_base - 1] = (total + (2 << bsize_log)) >> (bsize_log + 2);
    }
#else
  } else if (mode >= MODE_DC) {
    const int step = reduction_stride(loc_base, wi, wi_count, mode == MODE_CFL ? wi_count : dc_terms);
    if (step > 1 || wi == 0) {
      const uint count = dc_above * 2 + dc_left;
      const uint dc = sum_partials(EDGE_ABOVE, loc_base, wi, dc_terms, step);
      if (wi == 0) edges[EDGE_ABOVE][loc_base - 1] = count ? (dc + (count >> 1)) / count : 512;

      if (mode == MODE_CFL) {
        const int total = sum_partials(EDGE_LEFT, loc_base, wi, wi_count, step);
        if (wi == 0) edges[EDGE_LEFT][loc_base - 1] = (total + (2 << bsize_log)) >> (bsize_log + 2);
      }
    }
#endif
  }

  GroupMemoryBarrierWithGroupSync();
  if (directional) {
    write_edge(EDGE_ABOVE, loc_base, wi, wi_count, above_count, upsample_above,
               filter_above, strength_above, next_above);
    write_edge(EDGE_LEFT, loc_base, wi, wi_count, left_count, upsample_left,
               filter_left, strength_left, next_left);
  }

  GroupMemoryBarrierWithGroupSync();
  if (thread.x >= cb_wi_count) return;

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
               (compute_bc((ref0.x >> 16) & 1023, (ref0.y >> 0) & 1023, (ref1.x >> 16) & 1023, (ref1.y >> 0) & 1023,
                           filt_h, filt_v)
                << 16);
    pixels.y = (compute_bc((ref0.y >> 0) & 1023, (ref0.y >> 16) & 1023, (ref1.y >> 0) & 1023, (ref1.y >> 16) & 1023,
                           filt_h, filt_v)
                << 0) |
               (compute_bc((ref0.y >> 16) & 1023, (ref0.z >> 0) & 1023, (ref1.y >> 16) & 1023, (ref1.z >> 0) & 1023,
                           filt_h, filt_v)
                << 16);
  }

  if (mode >= MODE_DC) {
    const uint dc = edges[EDGE_ABOVE][loc_base - 1];
    if (mode == MODE_CFL) {
      const int avrg = edges[EDGE_LEFT][loc_base - 1];
      const int alpha = ((block.y >> 22) & 63) - 16;
      pixels.x = compute_cfl_pixel(dc, (luma.x - avrg) * alpha);
      pixels.x |= compute_cfl_pixel(dc, (luma.y - avrg) * alpha) << 16;
      pixels.y = compute_cfl_pixel(dc, (luma.z - avrg) * alpha);
      pixels.y |= compute_cfl_pixel(dc, (luma.w - avrg) * alpha) << 16;
    } else {
      pixels.x = dc | (dc << 16);
      pixels.y = pixels.x;
    }
  } else if (mode < MODE_INTRA_BC) {
    if (directional) {
      const int frac_bits_x = 6 - upsample_above;
      const int frac_bits_y = 6 - upsample_left;
      const int min_base_x = -(1 << upsample_above);
      const int last_x = (above_count << upsample_above) - 1;
      const int last_y = (left_count << upsample_left) - 1;
      const int dx = (y + 1) * params.x;
      const int frac_x = ((dx << upsample_above) & 63) >> 1;
      [loop] for (int i = 0; i < 4; ++i) {
        const int x = x0 + i;
        const int base_x = (x << upsample_above) + (dx >> frac_bits_x);
        const bool from_above = mode_info == Dir1 || (mode_info == Dir2 && base_x >= min_base_x);
        const int dy = (y << 6) + (x + 1) * params.y;
        const int base = from_above ? base_x : dy >> frac_bits_y;
        const int fraction = from_above ? frac_x : ((dy << upsample_left) & 63) >> 1;
        const int edge = from_above ? EDGE_ABOVE : EDGE_LEFT;
        const int last = from_above ? last_x : last_y;
        const int p0 = edges[edge][loc_base + clamp(base, -2, last)];
        int val = p0;
        if (fraction) {
          const int p1 = edges[edge][loc_base + clamp(base + 1, -2, last)];
          val = p0 + ((fraction * (p1 - p0) + 16) >> 5);
        }
        const uint packed = (uint)clamp(val, 0, 1023) << ((i & 1) * 16);
        if (i < 2) pixels.x |= packed;
        else pixels.y |= packed;
      }
    } else if (mode == MODE_PAETH) {
      const int left = edges[EDGE_LEFT][loc_base + y];
      const int corner_pixel = edges[EDGE_ABOVE][loc_base - 1];
      const int diff_top = corner_pixel - left;
      const int dt = abs(diff_top);
      [loop] for (int i = 0; i < 4; ++i) {
        const int top = edges[EDGE_ABOVE][loc_base + x0 + i];
        const int diff_left = corner_pixel - top;
        const int diff_corner = abs(diff_top + diff_left);
        const int dl = abs(diff_left);
        const int val = dl <= dt && dl <= diff_corner ? left : dt <= diff_corner ? top : corner_pixel;
        const uint packed = (uint)clamp(val, 0, 1023) << ((i & 1) * 16);
        if (i < 2) pixels.x |= packed;
        else pixels.y |= packed;
      }
    } else {
      // Smooth vertical and horizontal each contribute a weighted pair. The vertical weight
      // and bottom sample are common to the row.
      int vertical_weight = 0;
      int bottom_term = 0;
      int left = 0;
      int right = 0;
      if (mode_info & SmoothV) {
        vertical_weight = cb_sm_weight_arrays[bh + y].x;
        bottom_term = (256 - vertical_weight) * edges[EDGE_LEFT][loc_base + bh - 1];
      }
      if (mode_info & SmoothH) {
        left = edges[EDGE_LEFT][loc_base + y];
        right = edges[EDGE_ABOVE][loc_base + bw - 1];
      }
      [loop] for (int i = 0; i < 4; ++i) {
        const int x = x0 + i;
        int sum = bottom_term;
        if (mode_info & SmoothV) sum += vertical_weight * edges[EDGE_ABOVE][loc_base + x];
        if (mode_info & SmoothH) sum += 256 * right + cb_sm_weight_arrays[bw + x].x * (left - right);
        const uint packed = (uint)clamp((sum + (1 << (params.w - 1))) >> params.w, 0, 1023) << ((i & 1) * 16);
        if (i < 2) pixels.x |= packed;
        else pixels.y |= packed;
      }
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
