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

#include "restoration.h"

#define Round2(value, n) (((value) + (((1 << (n)) >> 1))) >> (n))
#define STRIPE_SIZE 64
#define RESTORE_NONE 0
#define RESTORE_WIENER 1
#define RESTORE_SGRPROJ 2
#define RESTORE_SWITCHABLE 3
#define RESTORE_SWITCHABLE_TYPES RESTORE_SWITCHABLE
#define RESTORE_TYPES 4
#define WIENER_ROUND0_BITS 3
#define FILTER_BITS 7
#define InterRound0 WIENER_ROUND0_BITS
#define InterRound1 (2 * FILTER_BITS - WIENER_ROUND0_BITS)

ByteAddressBuffer src : register(t0);        // SRV
ByteAddressBuffer LrTypeSgr : register(t1);  // SRV
ByteAddressBuffer LrWiener : register(t2);   // SRV
RWByteAddressBuffer dst : register(u0);      // UAV

cbuffer PlaneRestorationData : register(b0) {
  struct {
    int4 Sgr_Params[16];
    uint4 Sgr_A2Packed[16];
  } data;
};

struct PlaneInfo {
  int stride;
  int offset;
  int width;
  int height;
};

struct UnitsInfo {
  int Rows;
  int Cols;
  int Size;
  int Stride;
};

struct PlaneRestorationData {
  PlaneInfo plane;
  UnitsInfo units;

  int pp_offset;
  int dst_offset;
  int Lr_buffer_offset;
  int subsampling;
  int hbd;
  int bit_depth;
  int unit_shift;  // log2(units.Size), in the first word of padding in the legacy layout
  int pad;
};
cbuffer PlaneRestorationConstBuffer : register(b1) { PlaneRestorationData pl[3]; };

cbuffer cb_loop_rest_data : register(b2) {
  int do_restoration;
  int plane_id;
}

#define WG_WIDTH 16
#define WG_HEIGHT 4
#define BOX_WIDTH (WG_WIDTH + 2)
#define BOX_HEIGHT (WG_HEIGHT + 2)

groupshared int input[WG_HEIGHT + 6][WG_WIDTH + 8];
groupshared int output[WG_HEIGHT][WG_WIDTH];
groupshared int intermediate[WG_HEIGHT + 6][WG_WIDTH];

groupshared int flt[2][WG_HEIGHT][WG_WIDTH];
groupshared int A[WG_HEIGHT + 2][WG_WIDTH + 2];
groupshared int B[WG_HEIGHT + 2][WG_WIDTH + 2];
groupshared uint sgr_a2[64];

// round(2^SGRPROJ_RECIP_BITS / n), where n is the area of the box.
static const uint SGR_RECIP_5X5 = ((1 << SGRPROJ_RECIP_BITS) + 12) / 25;
static const uint SGR_RECIP_3X3 = ((1 << SGRPROJ_RECIP_BITS) + 4) / 9;

uint lookup_sgr_a2(uint z) {
  const uint word = sgr_a2[z >> 2];
  return ((word >> ((z & 3) * 8)) & 255) + 1;
}

#define get_loaded_source_sample(x, y) input[y + 3][x + 4]
// Self guided filter, pass 0, first half: the box sums A and B of the group (with a border of one), spread
// over its threads.
void box_sums0(uint id, int eps, uint2 shifts, uint2 rounding) {
  for (uint idx = id; idx < BOX_HEIGHT * BOX_WIDTH; idx += WG_HEIGHT * WG_WIDTH) {
    uint row = idx / BOX_WIDTH;
    int i = (int)row - 1;
    int j = (int)(idx - row * BOX_WIDTH) - 1;
    uint a = 0;
    uint b = 0;
    for (int dy = -2; dy <= 2; dy++) {
      for (int dx = -2; dx <= 2; dx++) {
        uint c = get_loaded_source_sample(j + dx, i + dy);
        a += c * c;
        b += c;
      }
    }
    a = (a + rounding.y) >> shifts.y;
    uint d = (b + rounding.x) >> shifts.x;
    uint p = max(0, int(a * 25 - d * d));
    uint z = Round2(p * eps, SGRPROJ_MTABLE_BITS);  // p*s in documentation
    uint a2 = lookup_sgr_a2(min(z, 255u));
    uint b2 = ((1 << SGRPROJ_SGR_BITS) - a2) * b * SGR_RECIP_5X5;
    A[1 + i][1 + j] = a2;
    B[1 + i][1 + j] = Round2(b2, SGRPROJ_RECIP_BITS);
  }
}

// Second half: the filtered sample of a thread, from the box sums around it.
int box_filter0(int lx, int ly) {
  //for (int i = ly; i < h; i += WG_HEIGHT) {
  { int i = ly;
    int shift = 5;  // -((1 - stage) * (i & 1));
    if (i & 1) {
      shift = 4;
    }
    //for (int j = lx; j < w; j += WG_WIDTH) {
    { int j = lx;
      int a = 0;
      int b = 0;
      for (int dy = -1; dy <= 1; dy++) {
        for (int dx = -1; dx <= 1; dx++) {
          int weight = 0;
          if ((i + dy) & 1) {
            weight = (dx == 0) ? 6 : 5;
          } else {
            weight = 0;
          }
          a += weight * A[1 + i + dy][1 + j + dx];
          b += weight * B[1 + i + dy][1 + j + dx];
        }
      }
      int v = a * get_loaded_source_sample(j, i) + b;
      return Round2(v, SGRPROJ_SGR_BITS + shift - SGRPROJ_RST_BITS);
    }
  }
}

// Self guided filter, pass 1, first half: the box sums A and B of the group (with a border of one), spread
// over its threads.
void box_sums1(uint id, int eps, uint2 shifts, uint2 rounding) {
  for (uint idx = id; idx < BOX_HEIGHT * BOX_WIDTH; idx += WG_HEIGHT * WG_WIDTH) {
    uint row = idx / BOX_WIDTH;
    int i = (int)row - 1;
    int j = (int)(idx - row * BOX_WIDTH) - 1;
    uint a = 0;
    uint b = 0;
    for (int dy = -1; dy <= 1; dy++) {
      for (int dx = -1; dx <= 1; dx++) {
        uint c = get_loaded_source_sample(j + dx, i + dy);
        a += c * c;
        b += c;
      }
    }
    a = (a + rounding.y) >> shifts.y;
    uint d = (b + rounding.x) >> shifts.x;
    uint p = max(0, int(a * 9 - d * d));
    uint z = Round2(p * eps, SGRPROJ_MTABLE_BITS);  // p*s in documentation
    uint a2 = lookup_sgr_a2(min(z, 255u));
    uint b2 = ((1 << SGRPROJ_SGR_BITS) - a2) * b * SGR_RECIP_3X3;
    A[1 + i][1 + j] = a2;
    B[1 + i][1 + j] = Round2(b2, SGRPROJ_RECIP_BITS);
  }
}

// Second half: the filtered sample of a thread, from the box sums around it.
int box_filter1(int lx, int ly) {
  //for (i = ly; i < h; i += WG_HEIGHT) {
  { int i = ly;
    int shift = 5;  // -((1 - stage) * (i & 1));
    //for (int j = lx; j < w; j += WG_WIDTH) {
    { int j = lx;
      int a = 0;
      int b = 0;
      for (int dy = -1; dy <= 1; dy++) {
        for (int dx = -1; dx <= 1; dx++) {
          int weight = 0;
          weight = (dx == 0 || dy == 0) ? 4 : 3;
          a += weight * A[1 + i + dy][1 + j + dx];
          b += weight * B[1 + i + dy][1 + j + dx];
        }
      }
      int v = a * get_loaded_source_sample(j, i) + b;
      return Round2(v, SGRPROJ_SGR_BITS + shift - SGRPROJ_RST_BITS);
    }
  }
}

// One thread group restores 16x4 samples, one thread each. The work goes in passes, and a pass reads from
// the group memory what all threads of the group wrote in the pass before: the samples around the group,
// then the rows of the Wiener filter or the box sums of the self guided filter, then the restored samples,
// which one thread in four stores as words. The threads of a group run in no particular order, so there is
// a group sync between two passes. (Upstream had one plain group memory barrier, after the first pass,
// which makes no thread wait, and nothing between the others: that held only where the 64 threads of a
// group ran in lockstep, as one wave.) A group sync has to be reached by all threads of the group: which
// filter a group runs, and whether it leaves early, depends on the group only (the restoration unit it is
// in), not on the thread.
[numthreads(WG_WIDTH, WG_HEIGHT, 1)]
void main(uint3 local : SV_GroupThreadID, uint3 group : SV_GroupID, uint id : SV_GroupIndex) {
  const int gx = group.x * WG_WIDTH;
  const int gy = group.y * WG_HEIGHT;
  const int lx = local.x;
  const int ly = local.y;

  const int subsampling = pl[plane_id].subsampling;
  const bool hbd = pl[plane_id].hbd != 0;
  const PlaneInfo plane = pl[plane_id].plane;
  const int dst_offset = pl[plane_id].dst_offset;

  // Copy-only planes have a dummy grid. Skip their read of the root SRV.
  int unitId = 0;
  int4 rType = int4(RESTORE_NONE, 0, 0, 0);
  [branch] if (do_restoration) {
    UnitsInfo units = pl[plane_id].units;
    int shift = pl[plane_id].unit_shift;
    int unitCol = min(units.Cols - 1, uint(gx + 1) >> shift);
    int unitRow = min(units.Rows - 1, uint(gy + 1 + (8 >> subsampling)) >> shift);
    unitId = unitRow * units.Stride + unitCol + pl[plane_id].Lr_buffer_offset;
    rType = LrTypeSgr.Load4(unitId * 16);
  }

  if (rType.x == RESTORE_NONE) {
    if ((lx & 3) == 0) {
      const int row_offset = (gy + ly) * plane.stride;
      if (hbd) {
        int at = row_offset + ((gx + lx) << 1);
        dst.Store2(dst_offset + at, src.Load2(plane.offset + at));
      } else {
        int at = row_offset + gx + lx;
        dst.Store(dst_offset + at, src.Load(plane.offset + at));
      }
    }
    return;
  }

  const int stripe_id = uint((gy << subsampling) + 8) / STRIPE_SIZE;
  const int StripeStartY = ((stripe_id * STRIPE_SIZE) - 8) >> subsampling;
  const int StripeEndY = StripeStartY + (STRIPE_SIZE >> subsampling) - 1;
  const int pp_offset = pl[plane_id].pp_offset;
  const int bit_depth = pl[plane_id].bit_depth;
  const int sample_max = (1 << bit_depth) - 1;

  if (lx < WG_WIDTH / 4 + 2) {
    const int nc_x = (lx << 2) + gx - 4;
    const int c_x = clamp(nc_x, 0, plane.width - 1) & ~3;
    const int last_sample = nc_x < 0 ? 0 : plane.width - c_x - 1;
    const int first_sample = nc_x - c_x >= 4 ? last_sample : 0;
    const int x = lx;
    for (int y = ly; y < WG_HEIGHT + 6; y += WG_HEIGHT) {
      int c_y = clamp(y + gy - 3, StripeStartY - 2, StripeEndY + 2);
      c_y = clamp(c_y, 0, plane.height - 1);
      int offset = (c_y < StripeStartY || c_y > StripeEndY) ? pp_offset : plane.offset;
      offset += c_y * plane.stride;

      if (hbd) {
        int shift_max = last_sample * 16;
        int shift_min = first_sample * 16;
        uint2 input_char = src.Load2(offset + (c_x << 1));
        uint4 shift = uint4(clamp(0, shift_min, shift_max), clamp(16, shift_min, shift_max),
                            clamp(32, shift_min, shift_max), clamp(48, shift_min, shift_max));
        input[y][x * 4 + 0] = (shift.x > 16 ? (input_char.y >> (shift.x - 32)) : (input_char.x >> shift.x)) & 0xffff;
        input[y][x * 4 + 1] = (shift.y > 16 ? (input_char.y >> (shift.y - 32)) : (input_char.x >> shift.y)) & 0xffff;
        input[y][x * 4 + 2] = (shift.z > 16 ? (input_char.y >> (shift.z - 32)) : (input_char.x >> shift.z)) & 0xffff;
        input[y][x * 4 + 3] = (shift.w > 16 ? (input_char.y >> (shift.w - 32)) : (input_char.x >> shift.w)) & 0xffff;
      } else {
        int shift_max = last_sample * 8;
        int shift_min = first_sample * 8;
        uint input_char = src.Load(offset + c_x);
        input[y][x * 4 + 0] = (input_char >> clamp(0, shift_min, shift_max)) & 255;
        input[y][x * 4 + 1] = (input_char >> clamp(8, shift_min, shift_max)) & 255;
        input[y][x * 4 + 2] = (input_char >> clamp(16, shift_min, shift_max)) & 255;
        input[y][x * 4 + 3] = (input_char >> clamp(24, shift_min, shift_max)) & 255;
      }
    }
  }

  // The first group sync also publishes the a2 table. Lookups in the box passes use group memory.
  if (rType.x == RESTORE_SGRPROJ && ly == 0) {
    const uint4 packed = data.Sgr_A2Packed[lx];
    sgr_a2[4 * lx] = packed.x;
    sgr_a2[4 * lx + 1] = packed.y;
    sgr_a2[4 * lx + 2] = packed.z;
    sgr_a2[4 * lx + 3] = packed.w;
  }

  GroupMemoryBarrierWithGroupSync();

  if (rType.x == RESTORE_WIENER) {
    const int limit = (1 << (bit_depth + 1 + FILTER_BITS - InterRound0)) - 1;
    const int horizontal_bias = 1 << (bit_depth + FILTER_BITS - 1);

    int Lr_offset = unitId * 64;
    int4 hfilter0 = LrWiener.Load4(Lr_offset + 0);
    int4 hfilter1 = LrWiener.Load4(Lr_offset + 16);

    for (int r = ly; r < WG_HEIGHT + 6; r += WG_HEIGHT) {
      int s = (input[r][lx + 4] << 7) + horizontal_bias;

      s += hfilter0.x * input[r][lx + 1];
      s += hfilter0.y * input[r][lx + 2];
      s += hfilter0.z * input[r][lx + 3];
      s += hfilter0.w * input[r][lx + 4];
      s += hfilter1.x * input[r][lx + 5];
      s += hfilter1.y * input[r][lx + 6];
      s += hfilter1.z * input[r][lx + 7];

      int v = Round2(s, InterRound0);
      intermediate[r][lx] = clamp(v, 0, limit);
    }

    GroupMemoryBarrierWithGroupSync();

    int4 vfilter0 = LrWiener.Load4(Lr_offset + 32);
    int4 vfilter1 = LrWiener.Load4(Lr_offset + 48);

    int s = (intermediate[ly + 3][lx] << 7) - (1 << (bit_depth + InterRound1 - 1));

    s += vfilter0.x * intermediate[ly + 0][lx];
    s += vfilter0.y * intermediate[ly + 1][lx];
    s += vfilter0.z * intermediate[ly + 2][lx];
    s += vfilter0.w * intermediate[ly + 3][lx];
    s += vfilter1.x * intermediate[ly + 4][lx];
    s += vfilter1.y * intermediate[ly + 5][lx];
    s += vfilter1.z * intermediate[ly + 6][lx];
    int v = Round2(s, InterRound1);
    output[ly][lx] = clamp(v, 0, sample_max);

    GroupMemoryBarrierWithGroupSync();

    if (lx < WG_WIDTH / 4) {
      if (hbd) {
        dst.Store2(dst_offset + (gy + ly) * plane.stride + (gx + lx * 4) * 2,
                   uint2((output[ly][lx * 4 + 0] << 0) | (output[ly][lx * 4 + 1] << 16),
                         (output[ly][lx * 4 + 2] << 0) | (output[ly][lx * 4 + 3] << 16)));
      } else {
        dst.Store(dst_offset + (gy + ly) * plane.stride + gx + lx * 4,
                  (output[ly][lx * 4 + 0] << 0) | (output[ly][lx * 4 + 1] << 8) | (output[ly][lx * 4 + 2] << 16) |
                      (output[ly][lx * 4 + 3] << 24));
      }
    }
  } else if (rType.x == RESTORE_SGRPROJ) {
    const int w0 = rType.y;
    const int w1 = rType.z;
    const int w2 = (1 << SGRPROJ_PRJ_BITS) - w0 - w1;

    const int4 params = data.Sgr_Params[rType.w];
    const int sample_shift = bit_depth - 8;
    const uint2 shifts = uint2(sample_shift, 2 * sample_shift);
    const uint2 rounding = (uint2(1, 1) << shifts) >> 1u;

    int u = input[ly + 3][lx + 4] << SGRPROJ_RST_BITS;
    int v = w1 * u;
    // The two passes use the same memory for their box sums: a group sync before the sums are read, and
    // one before the next pass writes its own.
    if (params.x) {
        box_sums0(id, params.z, shifts, rounding);
        GroupMemoryBarrierWithGroupSync();
        v += w0 * box_filter0(lx, ly);
        GroupMemoryBarrierWithGroupSync();
    } else {
        v += w0 * u;
    }
    if (params.y) {
        box_sums1(id, params.w, shifts, rounding);
        GroupMemoryBarrierWithGroupSync();
        v += w2 * box_filter1(lx, ly);
    } else {
        v += w2 * u;
    }

    int s = Round2(v, (SGRPROJ_RST_BITS + SGRPROJ_PRJ_BITS));
    output[ly][lx] = clamp(s, 0, sample_max);

    GroupMemoryBarrierWithGroupSync();

    if (lx < WG_WIDTH / 4) {
      if (hbd) {
        dst.Store2(dst_offset + (gy + ly) * plane.stride + (gx + lx * 4) * 2,
                   uint2((output[ly][lx * 4 + 0] << 0) | (output[ly][lx * 4 + 1] << 16),
                         (output[ly][lx * 4 + 2] << 0) | (output[ly][lx * 4 + 3] << 16)));
      } else {
        dst.Store(dst_offset + (gy + ly) * plane.stride + gx + lx * 4,
                  (output[ly][lx * 4 + 0] << 0) | (output[ly][lx * 4 + 1] << 8) | (output[ly][lx * 4 + 2] << 16) |
                      (output[ly][lx * 4 + 3] << 24));
      }
    }
  }
}
