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
#define WG_THREADS (WG_WIDTH * WG_HEIGHT)
#define INPUT_WIDTH (WG_WIDTH + 8)
#define INPUT_HEIGHT (WG_HEIGHT + 6)
#define INPUT_WORDS (INPUT_WIDTH * INPUT_HEIGHT)

#ifdef LR_WIENER_ONLY
#define WORK_WORDS (INPUT_WORDS + WG_WIDTH * INPUT_HEIGHT)
#else
#define BOX_WIDTH (WG_WIDTH + 2)
#define BOX_HEIGHT (WG_HEIGHT + 2)
#define COLUMN5_WIDTH (BOX_WIDTH + 4)
#define COLUMN3_WIDTH (BOX_WIDTH + 2)
#define COLUMN5_COUNT (BOX_HEIGHT / 2 * COLUMN5_WIDTH)
#define COLUMN3_COUNT (BOX_HEIGHT * COLUMN3_WIDTH)
#define COLUMN5_BASE INPUT_WORDS
#define COLUMN3_BASE (COLUMN5_BASE + 2 * COLUMN5_COUNT)
#define A2_BASE (COLUMN3_BASE + 2 * COLUMN3_COUNT)
#define WORK_WORDS (A2_BASE + 64)
#define BOX3_COUNT (BOX_HEIGHT * BOX_WIDTH)
#endif

// The first region holds the input tile. The space after it holds the Wiener intermediate
// or the SGR column sums and lookup table. SGR can recycle the input once all columns are ready.
groupshared uint work[WORK_WORDS];
groupshared int output[WG_HEIGHT][WG_WIDTH];
#undef WORK_WORDS

int input_sample(uint x, uint y) {
  return int(work[y * INPUT_WIDTH + x]);
}

uint wiener_index(uint x, uint y) {
  return INPUT_WORDS + y * WG_WIDTH + x;
}

int wiener_sample(uint x, uint y) {
  return int(work[wiener_index(x, y)]);
}

int wiener_filter(uint lx, uint ly, int unit_id, int bit_depth) {
  const int limit = (1 << (bit_depth + 1 + FILTER_BITS - InterRound0)) - 1;
  const int horizontal_bias = 1 << (bit_depth + FILTER_BITS - 1);
  const int offset = unit_id * 64;
  const int4 hfilter0 = LrWiener.Load4(offset + 0);
  const int4 hfilter1 = LrWiener.Load4(offset + 16);

  for (uint r = ly; r < INPUT_HEIGHT; r += WG_HEIGHT) {
    int s = (input_sample(lx + 4, r) << 7) + horizontal_bias;
    s += hfilter0.x * input_sample(lx + 1, r);
    s += hfilter0.y * input_sample(lx + 2, r);
    s += hfilter0.z * input_sample(lx + 3, r);
    s += hfilter0.w * input_sample(lx + 4, r);
    s += hfilter1.x * input_sample(lx + 5, r);
    s += hfilter1.y * input_sample(lx + 6, r);
    s += hfilter1.z * input_sample(lx + 7, r);
    work[wiener_index(lx, r)] = clamp(Round2(s, InterRound0), 0, limit);
  }

  GroupMemoryBarrierWithGroupSync();

  const int4 vfilter0 = LrWiener.Load4(offset + 32);
  const int4 vfilter1 = LrWiener.Load4(offset + 48);
  int s = (wiener_sample(lx, ly + 3) << 7) - (1 << (bit_depth + InterRound1 - 1));
  s += vfilter0.x * wiener_sample(lx, ly + 0);
  s += vfilter0.y * wiener_sample(lx, ly + 1);
  s += vfilter0.z * wiener_sample(lx, ly + 2);
  s += vfilter0.w * wiener_sample(lx, ly + 3);
  s += vfilter1.x * wiener_sample(lx, ly + 4);
  s += vfilter1.y * wiener_sample(lx, ly + 5);
  s += vfilter1.z * wiener_sample(lx, ly + 6);
  return Round2(s, InterRound1);
}

#ifndef LR_WIENER_ONLY
// round(2^SGRPROJ_RECIP_BITS / n), where n is the area of the box.
static const uint SGR_RECIP_5X5 = ((1 << SGRPROJ_RECIP_BITS) + 12) / 25;
static const uint SGR_RECIP_3X3 = ((1 << SGRPROJ_RECIP_BITS) + 4) / 9;

uint lookup_sgr_a2(uint z) {
  const uint word = work[A2_BASE + (z >> 2)];
  return ((word >> ((z & 3) * 8)) & 255) + 1;
}

uint2 source_column3(uint x, uint y) {
  const uint3 samples = uint3(input_sample(x, y), input_sample(x, y + 1), input_sample(x, y + 2));
  const uint3 squares = samples * samples;
  return uint2(samples.x + samples.y + samples.z, squares.x + squares.y + squares.z);
}

void store_column(uint at, uint count, uint2 sums) {
  work[at] = sums.x;
  work[at + count] = sums.y;
}

uint2 load_column(uint at, uint count) {
  return uint2(work[at], work[at + count]);
}

void vertical_sums(uint id, bool use3) {
  // The radius-two filter consumes only coefficient rows centered at -1, 1 and 3.
  // Build their five-row column sums and reuse the middle three rows for radius one.
  for (uint idx = id; idx < COLUMN5_COUNT; idx += WG_THREADS) {
    const uint row = idx / COLUMN5_WIDTH;
    const uint x = idx - row * COLUMN5_WIDTH;
    const uint y = 2 * row;
    const uint2 middle = source_column3(x + 1, y + 1);
    const uint above = input_sample(x + 1, y);
    const uint below = input_sample(x + 1, y + 4);
    store_column(COLUMN5_BASE + idx, COLUMN5_COUNT, middle + uint2(above + below, above * above + below * below));
    if (use3 && x > 0 && x < COLUMN5_WIDTH - 1) {
      store_column(COLUMN3_BASE + y * COLUMN3_WIDTH + x - 1, COLUMN3_COUNT, middle);
    }
  }
  // The remaining radius-one centers are 0, 2 and 4. Their tile has one less halo column on each side.
  if (use3 && id < COLUMN3_COUNT / 2) {
    const uint row = id / COLUMN3_WIDTH;
    const uint x = id - row * COLUMN3_WIDTH;
    const uint y = 2 * row + 1;
    store_column(COLUMN3_BASE + y * COLUMN3_WIDTH + x, COLUMN3_COUNT, source_column3(x + 2, y + 1));
  }
}

uint pack_box(uint2 sums, uint area, uint reciprocal, uint strength, uint2 shifts, uint2 rounding) {
  const uint2 normalized = (sums + rounding) >> shifts;
  const uint variance = max(0, int(normalized.y * area - normalized.x * normalized.x));
  const uint z = Round2(variance * strength, SGRPROJ_MTABLE_BITS);
  const uint a2 = lookup_sgr_a2(min(z, 255u));
  const uint b2 = ((1 << SGRPROJ_SGR_BITS) - a2) * sums.x * reciprocal;
  // a2 can equal 256. Rounded b2 is bounded by 256 times the largest sample and fits above it.
  return a2 | (Round2(b2, SGRPROJ_RECIP_BITS) << 9);
}

uint2 load_box(uint at) {
  const uint packed = work[at];
  return uint2(packed & 511, packed >> 9);
}

void box_sums5(uint id, uint strength, uint2 shifts, uint2 rounding) {
  if (id < BOX3_COUNT / 2) {
    const uint y = id / BOX_WIDTH;
    const uint x = id - y * BOX_WIDTH;
    const uint column = COLUMN5_BASE + y * COLUMN5_WIDTH + x;
    uint2 sums = uint2(0, 0);
    [unroll] for (uint dx = 0; dx < 5; ++dx) {
      sums += load_column(column + dx, COLUMN5_COUNT);
    }
    work[BOX3_COUNT + id] = pack_box(sums, 25u, SGR_RECIP_5X5, strength, shifts, rounding);
  }
}

void box_sums3(uint id, bool reuse_columns, uint box_base, uint strength, uint2 shifts, uint2 rounding) {
  for (uint idx = id; idx < BOX3_COUNT; idx += WG_THREADS) {
    const uint y = idx / BOX_WIDTH;
    const uint x = idx - y * BOX_WIDTH;
    uint2 sums = uint2(0, 0);
    [branch] if (reuse_columns) {
      const uint column = COLUMN3_BASE + y * COLUMN3_WIDTH + x;
      [unroll] for (uint dx = 0; dx < 3; ++dx) {
        sums += load_column(column + dx, COLUMN3_COUNT);
      }
    } else {
      [unroll] for (uint dx = 0; dx < 3; ++dx) {
        sums += source_column3(x + dx + 2, y + 1);
      }
    }
    work[box_base + idx] = pack_box(sums, 9u, SGR_RECIP_3X3, strength, shifts, rounding);
  }
}

uint2 filter5_row(uint row, uint x) {
  const uint at = BOX3_COUNT + row * BOX_WIDTH + x;
  return 5u * (load_box(at) + load_box(at + 2)) + 6u * load_box(at + 1);
}

int box_filter5(uint x, uint y, uint sample) {
  const uint row = y / 2;
  const bool even = (y & 1) == 0;
  uint2 coefficients = filter5_row(row + 1, x);
  if (even) {
    coefficients += filter5_row(row, x);
  }
  const uint shift = even ? 5 : 4;
  const uint v = coefficients.x * sample + coefficients.y;
  return Round2(v, SGRPROJ_SGR_BITS + shift - SGRPROJ_RST_BITS);
}

int box_filter3(uint x, uint y, uint sample, uint box_base) {
  uint2 coefficients = uint2(0, 0);
  [unroll] for (uint dy = 0; dy < 3; ++dy) {
    [unroll] for (uint dx = 0; dx < 3; ++dx) {
      const uint2 box = load_box(box_base + (y + dy) * BOX_WIDTH + x + dx);
      coefficients += (dx == 1 || dy == 1 ? 4u : 3u) * box;
    }
  }
  const uint v = coefficients.x * sample + coefficients.y;
  return Round2(v, SGRPROJ_SGR_BITS + 5 - SGRPROJ_RST_BITS);
}

int sgr_filter(uint lx, uint ly, uint id, int4 rType, int bit_depth) {
  const int4 params = data.Sgr_Params[rType.w];
  const int sample_shift = bit_depth - 8;
  const uint2 shifts = uint2(sample_shift, 2 * sample_shift);
  const uint2 rounding = (uint2(1, 1) << shifts) >> 1u;

  const bool use5 = params.x != 0;
  const bool use3 = params.y != 0;
  const int sample = input_sample(lx + 4, ly + 3);
  const int u = sample << SGRPROJ_RST_BITS;

  // With radius two, the column barrier also ends all input reads. Without it,
  // write radius-one coefficients into the unused column area.
  const uint box_base = use5 ? 0 : COLUMN5_BASE;
  if (use5) {
    vertical_sums(id, use3);
    GroupMemoryBarrierWithGroupSync();
    box_sums5(id, params.z, shifts, rounding);
  }
  if (use3) {
    box_sums3(id, use5, box_base, params.w, shifts, rounding);
  }
  GroupMemoryBarrierWithGroupSync();

  int filtered5 = u;
  int filtered3 = u;
  [branch] if (use5) {
    filtered5 = box_filter5(lx, ly, sample);
  }
  [branch] if (use3) {
    filtered3 = box_filter3(lx, ly, sample, box_base);
  }
  const int w0 = rType.y;
  const int w1 = rType.z;
  const int w2 = (1 << SGRPROJ_PRJ_BITS) - w0 - w1;
  const int v = w1 * u + w0 * filtered5 + w2 * filtered3;
  return Round2(v, SGRPROJ_RST_BITS + SGRPROJ_PRJ_BITS);
}
#endif

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
#ifdef LR_WIENER_ONLY
    rType.x = LrTypeSgr.Load(unitId * 16);
#else
    rType = LrTypeSgr.Load4(unitId * 16);
#endif
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
    for (int y = ly; y < INPUT_HEIGHT; y += WG_HEIGHT) {
      const int at = y * INPUT_WIDTH + 4 * lx;
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
        work[at + 0] = (shift.x > 16 ? (input_char.y >> (shift.x - 32)) : (input_char.x >> shift.x)) & 0xffff;
        work[at + 1] = (shift.y > 16 ? (input_char.y >> (shift.y - 32)) : (input_char.x >> shift.y)) & 0xffff;
        work[at + 2] = (shift.z > 16 ? (input_char.y >> (shift.z - 32)) : (input_char.x >> shift.z)) & 0xffff;
        work[at + 3] = (shift.w > 16 ? (input_char.y >> (shift.w - 32)) : (input_char.x >> shift.w)) & 0xffff;
      } else {
        int shift_max = last_sample * 8;
        int shift_min = first_sample * 8;
        uint input_char = src.Load(offset + c_x);
        work[at + 0] = (input_char >> clamp(0, shift_min, shift_max)) & 255;
        work[at + 1] = (input_char >> clamp(8, shift_min, shift_max)) & 255;
        work[at + 2] = (input_char >> clamp(16, shift_min, shift_max)) & 255;
        work[at + 3] = (input_char >> clamp(24, shift_min, shift_max)) & 255;
      }
    }
  }

#ifndef LR_WIENER_ONLY
  // The first group sync also publishes the a2 table. Lookups in the box passes use group memory.
  if (rType.x == RESTORE_SGRPROJ && ly == 0) {
    const uint4 packed = data.Sgr_A2Packed[lx];
    work[A2_BASE + 4 * lx] = packed.x;
    work[A2_BASE + 4 * lx + 1] = packed.y;
    work[A2_BASE + 4 * lx + 2] = packed.z;
    work[A2_BASE + 4 * lx + 3] = packed.w;
  }
#endif

  GroupMemoryBarrierWithGroupSync();

#ifdef LR_WIENER_ONLY
  (void)id;
  const int filtered = wiener_filter(lx, ly, unitId, bit_depth);
#else
  int filtered;
  [branch] if (rType.x == RESTORE_WIENER) {
    filtered = wiener_filter(lx, ly, unitId, bit_depth);
  } else if (rType.x == RESTORE_SGRPROJ) {
    filtered = sgr_filter(lx, ly, id, rType, bit_depth);
  } else {
    return;
  }
#endif
  output[ly][lx] = clamp(filtered, 0, sample_max);
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
