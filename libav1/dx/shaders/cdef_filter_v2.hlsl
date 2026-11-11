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

// CDEF, second version (test player flag -cdefv2). Same resources, constant
// buffer and output bits as cdef_filter.hlsl, different work split:
//
//  * one group per 64x64 luma block (one cdef_index entry), 64 threads, one
//    thread per 8x8 luma block and its two 4x4 chroma blocks;
//  * a 64x64 block whose luma and chroma strengths are both zero (or whose
//    index is outside 0..15) is copied straight from source to destination;
//  * otherwise the group loads the luma window and both chroma windows once
//    into group shared memory, two 16-bit samples per word, and syncs once;
//  * the direction search runs on every thread for its own block, fully
//    unrolled with constant array indices, so it stays in registers;
//  * each thread filters its block and stores the result itself, there is no
//    shared output buffer.
//
// All sample maths is the 32-bit integer maths of cdef_filter.hlsl, including
// what that shader does outside the frame, so the stored bytes are identical.
// This file is also compiled as C++ by testing/cdef_check, so it sticks to
// syntax both languages accept (the check strips attributes and semantics).

#pragma warning(disable : 4714)
ByteAddressBuffer cdef_index : register(t0);
ByteAddressBuffer skips : register(t1);
RWByteAddressBuffer dst : register(u0);

struct CDefData {
  int4 pl;

  int uv_stride;
  int dst_offset_y;
  int dst_offset_u;
  int dst_offset_v;

  int uv_offset_u;
  int uv_offset_v;
  int index_stride;
  int skips_stride;

  int pri_damping;
  int sec_damping;
  int pli;
  int hbd;

  int bit_depth;
  int3 _dummie;

  int4 cdef_directions[16][2];
  int4 cdef_strength[16];
  int4 cdef_uv_strength[16];
};

cbuffer cb_cdef_data : register(b0) { CDefData data; }

#define CDEF_VERY_LARGE (30000)
// Two CDEF_VERY_LARGE samples in one word.
#define CDEF_VERY_LARGE_X2 (0x75307530u)

// Window of one group, in samples. Luma: 64x64 plus 2 rows above and below and
// 4 columns left and right (loads are 4 samples wide). Chroma: 32x32 plus the
// same borders, once per chroma plane.
#define LUMA_STRIDE 72
#define LUMA_ROWS 68
#define CHROMA_STRIDE 40
#define CHROMA_ROWS 36
#define LUMA_WORDS (LUMA_ROWS * LUMA_STRIDE / 2)
#define CHROMA_WORDS (CHROMA_ROWS * CHROMA_STRIDE / 2)

groupshared uint window[LUMA_WORDS + 2 * CHROMA_WORDS];

// One 4-sample unit of a source plane, two samples per word. Same in-frame
// test and same masks as the loaders of cdef_filter.hlsl.
uint2 load_unit(int stride, int offset, int w4, int h, int x4, int y) {
  uint2 v = uint2(CDEF_VERY_LARGE_X2, CDEF_VERY_LARGE_X2);
  if (x4 >= 0 && y >= 0 && x4 < w4 && y < h) {
    if (data.hbd) {
      v = dst.Load2(offset + y * stride + (x4 << 3));
      v.x &= 0x03ff03ffu;
      v.y &= 0x03ff03ffu;
    } else {
      const uint c = dst.Load(offset + y * stride + x4 * 4);
      v.x = (c & 0xffu) | ((c & 0xff00u) << 8);
      v.y = ((c >> 16) & 0xffu) | ((c >> 24) << 16);
    }
  }
  return v;
}

// Stores one 4-sample unit at sample position (x, y) of a destination plane.
void store_unit(int offset, int stride, int x, int y, uint2 v) {
  if (data.hbd) {
    dst.Store2(offset + y * stride + (x << 1), v);
  } else {
    // Samples of blocks outside the frame can be wider than 8 bits; the
    // original ORs them together shifted, so this does too.
    dst.Store(offset + y * stride + x, (v.x & 0xffffu) | ((v.x >> 16) << 8) | ((v.y & 0xffffu) << 16) | ((v.y >> 16) << 24));
  }
}

// Sample s of the window (s counts samples, two per word).
int window_sample(int s) { return (int)((window[s >> 1] >> ((s & 1) << 4)) & 0xffffu); }

// get_block_dir_old() of cdef_filter.hlsl for the 8x8 block whose first word is
// w0. Every array index is a compile time constant once the loops are unrolled.
int2 block_dir(int w0, int coeff_shift) {
  int i;
  int j;
  int k;
  int n;
  int cost[8] = {0, 0, 0, 0, 0, 0, 0, 0};
  int partial[8][15] = {{0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0}, {0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0},
                        {0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0}, {0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0},
                        {0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0}, {0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0},
                        {0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0}, {0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0}};
  const int div_table[9] = {0, 840, 420, 280, 210, 168, 140, 120, 105};

  int best_cost = 0;
  int best_cost_2 = 0;
  int2 best_dir = {0, 0};
  [unroll] for (i = 0; i < 8; i++) {
    [unroll] for (n = 0; n < 4; n++) {
      const uint pair = window[w0 + i * (LUMA_STRIDE / 2) + n];
      [unroll] for (k = 0; k < 2; k++) {
        j = 2 * n + k;
        int x = (int)(k ? (pair >> 16) : (pair & 0xffffu));
        x = (x >> coeff_shift) - 128;
        partial[0][i + j] += x;
        partial[1][i + (j >> 1)] += x;
        partial[2][i] += x;
        partial[3][3 + i - (j >> 1)] += x;
        partial[4][7 + i - j] += x;
        partial[5][3 - (i >> 1) + j] += x;
        partial[6][j] += x;
        partial[7][(i >> 1) + j] += x;
      }
    }
  }
  [unroll] for (i = 0; i < 8; i++) {
    cost[2] += partial[2][i] * partial[2][i];
    cost[6] += partial[6][i] * partial[6][i];
  }
  cost[2] *= div_table[8];
  cost[6] *= div_table[8];
  [unroll] for (i = 0; i < 7; i++) {
    cost[0] += (partial[0][i] * partial[0][i] + partial[0][14 - i] * partial[0][14 - i]) * div_table[i + 1];
    cost[4] += (partial[4][i] * partial[4][i] + partial[4][14 - i] * partial[4][14 - i]) * div_table[i + 1];
  }
  cost[0] += partial[0][7] * partial[0][7] * div_table[8];
  cost[4] += partial[4][7] * partial[4][7] * div_table[8];
  [unroll] for (i = 1; i < 8; i += 2) {
    [unroll] for (j = 0; j < 4 + 1; j++) { cost[i] += partial[i][3 + j] * partial[i][3 + j]; }
    cost[i] *= div_table[8];
    [unroll] for (j = 0; j < 4 - 1; j++) {
      cost[i] += (partial[i][j] * partial[i][j] + partial[i][10 - j] * partial[i][10 - j]) * div_table[2 * j + 2];
    }
  }
  [unroll] for (i = 0; i < 8; i++) {
    if (cost[i] > best_cost) {
      best_cost = cost[i];
      best_cost_2 = cost[(i + 4) & 7];
      best_dir.x = i;
    }
  }
  best_dir.y = best_cost - best_cost_2;
  best_dir.y >>= 10;
  return best_dir;
}

int get_msb(unsigned int n) {
  int log = 0;
  unsigned int value = n;
  int i;

  [unroll] for (i = 4; i >= 0; --i) {
    const int shift = (1 << i);
    const unsigned int x = value >> shift;
    if (x != 0) {
      value = x;
      log += shift;
    }
  }
  return log;
}

// sign(diff) * min(abs(diff), max(0, threshold - (abs(diff) >> shift)))
int constrain(int diff, int threshold, int shift) {
  const int a = abs(diff);
  const int m = min(a, max(0, threshold - (a >> shift)));
  return diff < 0 ? -m : m;
}

int adjust_strength(int strength, int var) {
  const int i = var >> 6 ? min(get_msb(var >> 6), 12) : 0;
  return var ? (strength * (4 + i) + 8) >> 4 : 0;
}

// What one block needs to filter one plane. Tap offsets are in window samples.
struct BlockFilter {
  int pri_strength;
  int sec_strength;
  int pri_shift;
  int sec_shift;
  int pri_tap0;  // pri_taps[p_t][0]: 4 or 3
  int pri_tap1;  // pri_taps[p_t][1]: 2 or 3
  int pri_off0;
  int pri_off1;
  int sec_off0;
  int sec_off1;
  int sec_off2;
  int sec_off3;
};

BlockFilter block_filter(int pri_strength, int sec_strength, int dir, int damping, int coeff_shift, int stride) {
  BlockFilter f;
  const int p_t = (pri_strength >> coeff_shift) & 1;
  f.pri_strength = pri_strength;
  f.sec_strength = sec_strength;
  f.pri_shift = max(0, damping - get_msb(pri_strength));
  f.sec_shift = max(0, damping - get_msb(sec_strength));
  f.pri_tap0 = 4 - p_t;
  f.pri_tap1 = 2 + p_t;
  f.pri_off0 = data.cdef_directions[dir][0].y * stride + data.cdef_directions[dir][0].x;
  f.pri_off1 = data.cdef_directions[dir][1].y * stride + data.cdef_directions[dir][1].x;
  f.sec_off0 = data.cdef_directions[dir + 2][0].y * stride + data.cdef_directions[dir + 2][0].x;
  f.sec_off1 = data.cdef_directions[dir + 2][1].y * stride + data.cdef_directions[dir + 2][1].x;
  f.sec_off2 = data.cdef_directions[dir + 6][0].y * stride + data.cdef_directions[dir + 6][0].x;
  f.sec_off3 = data.cdef_directions[dir + 6][1].y * stride + data.cdef_directions[dir + 6][1].x;
  return f;
}

// The two taps at +offset and -offset around sample s.
#define CDEF_TAP_PAIR(offset, weight, strength, shift)                                             \
  {                                                                                                \
    const int q0 = window_sample(s + (offset));                                                    \
    const int q1 = window_sample(s - (offset));                                                    \
    sum += (weight) * (constrain(q0 - x, strength, shift) + constrain(q1 - x, strength, shift));  \
    if (q0 != CDEF_VERY_LARGE) mmax = max(q0, mmax);                                               \
    if (q1 != CDEF_VERY_LARGE) mmax = max(q1, mmax);                                               \
    mmin = min(mmin, min(q0, q1));                                                                 \
  }

// Filters the 4 samples that start at window sample s0 (always even) and
// returns them two per word.
uint2 filter_unit(int s0, BlockFilter f) {
  const uint2 centre = uint2(window[s0 >> 1], window[(s0 >> 1) + 1]);
  uint2 result = uint2(0u, 0u);
  int j;
  [unroll] for (j = 0; j < 4; j++) {
    const int s = s0 + j;
    const uint pair = j < 2 ? centre.x : centre.y;
    const int x = (int)((j & 1) ? (pair >> 16) : (pair & 0xffffu));
    int sum = 0;
    int mmax = x;
    int mmin = x;
    if (f.pri_strength) {
      CDEF_TAP_PAIR(f.pri_off0, f.pri_tap0, f.pri_strength, f.pri_shift)
      CDEF_TAP_PAIR(f.pri_off1, f.pri_tap1, f.pri_strength, f.pri_shift)
    }
    if (f.sec_strength) {
      CDEF_TAP_PAIR(f.sec_off0, 2, f.sec_strength, f.sec_shift)
      CDEF_TAP_PAIR(f.sec_off2, 2, f.sec_strength, f.sec_shift)
      CDEF_TAP_PAIR(f.sec_off1, 1, f.sec_strength, f.sec_shift)
      CDEF_TAP_PAIR(f.sec_off3, 1, f.sec_strength, f.sec_shift)
    }
    const uint y = (uint)clamp(x + ((8 + sum - (sum < 0)) >> 4), mmin, mmax);
    if (j < 2) {
      result.x |= y << ((j & 1) << 4);
    } else {
      result.y |= y << ((j & 1) << 4);
    }
  }
  return result;
}

[numthreads(8, 8, 1)] void main(uint3 Gid : SV_GroupID, uint3 GTid : SV_GroupThreadID) {
  const int lx = GTid.x;
  const int ly = GTid.y;
  // Luma and chroma position of the group and of this thread's block.
  const int gx = Gid.x * 64;
  const int gy = Gid.y * 64;
  const int guvx = Gid.x * 32;
  const int guvy = Gid.y * 32;
  const int x0 = gx + lx * 8;
  const int y0 = gy + ly * 8;
  const int uvx0 = guvx + lx * 4;
  const int uvy0 = guvy + ly * 4;
  const int coeff_shift = data.bit_depth - 8;
  const int damping = data.pri_damping + coeff_shift;
  // Source planes: width in 4-sample units and height, as the original tests them.
  const int w4_y = data.pl.z >> 2;
  const int h_y = data.pl.w;
  const int w4_uv = (data.pl.z >> 1) >> 2;
  const int h_uv = data.pl.w >> 1;
  // The original dispatches whole 32x32 luma groups; blocks past the last of
  // them are never written.
  const int in_extent = (x0 < (((data.pl.z + 31) >> 5) << 5)) && (y0 < (((data.pl.w + 31) >> 5) << 5));
  int r;
  int c;
  int u;
  int pid;

  const int index = cdef_index.Load((Gid.y * data.index_stride + Gid.x) * 4);
  uint strength_y = 0;
  uint strength_uv = 0;
  if (index >= 0 && index < 16) {
    strength_y = data.cdef_strength[index].x;
    strength_uv = data.cdef_uv_strength[index].x;
  }
  const int group_filters = (strength_y | strength_uv) != 0;

  if (!group_filters) {
    // Nothing to filter in this 64x64 block: copy, no shared memory.
    if (in_extent) {
      [loop] for (u = 0; u < 16; u++) {
        const int x = x0 + (u & 1) * 4;
        const int y = y0 + (u >> 1);
        store_unit(data.dst_offset_y, data.pl.x, x, y, load_unit(data.pl.x, data.pl.y, w4_y, h_y, x >> 2, y));
      }
      [loop] for (u = 0; u < 8; u++) {
        const int y = uvy0 + (u & 3);
        store_unit(u < 4 ? data.dst_offset_u : data.dst_offset_v, data.uv_stride, uvx0, y,
                   load_unit(data.uv_stride, u < 4 ? data.uv_offset_u : data.uv_offset_v, w4_uv, h_uv, uvx0 >> 2, y));
      }
    }
  } else {
    // Every unit of the three windows is loaded by exactly one thread.
    [loop] for (r = ly; r < LUMA_ROWS; r += 8) {
      [loop] for (c = lx; c < LUMA_STRIDE / 4; c += 8) {
        const uint2 v = load_unit(data.pl.x, data.pl.y, w4_y, h_y, (gx >> 2) - 1 + c, gy - 2 + r);
        window[r * (LUMA_STRIDE / 2) + c * 2] = v.x;
        window[r * (LUMA_STRIDE / 2) + c * 2 + 1] = v.y;
      }
    }
    [loop] for (pid = 0; pid < 2; pid++) {
      [loop] for (r = ly; r < CHROMA_ROWS; r += 8) {
        [loop] for (c = lx; c < CHROMA_STRIDE / 4; c += 8) {
          const uint2 v = load_unit(data.uv_stride, pid ? data.uv_offset_v : data.uv_offset_u, w4_uv, h_uv,
                                    (guvx >> 2) - 1 + c, guvy - 2 + r);
          window[LUMA_WORDS + pid * CHROMA_WORDS + r * (CHROMA_STRIDE / 2) + c * 2] = v.x;
          window[LUMA_WORDS + pid * CHROMA_WORDS + r * (CHROMA_STRIDE / 2) + c * 2 + 1] = v.y;
        }
      }
    }
  }
  GroupMemoryBarrierWithGroupSync();

  if (group_filters && in_extent) {
    const int skip = skips.Load(((y0 >> 3) * data.skips_stride + (x0 >> 3)) * 4);
    const int t_y = skip ? 0 : (int)(strength_y / 4);
    int s_y = skip ? 0 : (int)(strength_y % 4);
    const int t_uv = skip ? 0 : (int)(strength_uv / 4);
    int s_uv = skip ? 0 : (int)(strength_uv % 4);
    s_y += s_y == 3;
    s_uv += s_uv == 3;

    // First window sample of this thread's luma block and of its chroma block
    // in the U window.
    const int luma_s0 = (2 + ly * 8) * LUMA_STRIDE + 4 + lx * 8;
    const int chroma_s0 = LUMA_WORDS * 2 + (2 + ly * 4) * CHROMA_STRIDE + 4 + lx * 4;

    int2 dir_var = {0, 0};
    if (!skip && (t_y | t_uv) != 0) {
      dir_var = block_dir(luma_s0 >> 1, coeff_shift);
    }

    // Luma
    {
      const int pri_strength = adjust_strength(t_y << coeff_shift, dir_var.y);
      const int sec_strength = s_y << coeff_shift;
      const BlockFilter f =
          block_filter(pri_strength, sec_strength, t_y ? dir_var.x : 0, damping, coeff_shift, LUMA_STRIDE);
      // A skipped block and a block with no strength left are both a copy.
      const int filters = !skip && (pri_strength | sec_strength) != 0;
      [loop] for (u = 0; u < 16; u++) {
        const int s = luma_s0 + (u >> 1) * LUMA_STRIDE + (u & 1) * 4;
        uint2 v;
        if (filters) {
          v = filter_unit(s, f);
        } else {
          v = uint2(window[s >> 1], window[(s >> 1) + 1]);
        }
        store_unit(data.dst_offset_y, data.pl.x, x0 + (u & 1) * 4, y0 + (u >> 1), v);
      }
    }
    // Chroma
    {
      const int pri_strength = t_uv << coeff_shift;
      const int sec_strength = s_uv << coeff_shift;
      const BlockFilter f = block_filter(pri_strength, sec_strength, pri_strength ? dir_var.x : 0, damping - 1,
                                         coeff_shift, CHROMA_STRIDE);
      const int filters = !skip && (pri_strength | sec_strength) != 0;
      [loop] for (u = 0; u < 8; u++) {
        const int s = chroma_s0 + (u >> 2) * (CHROMA_WORDS * 2) + (u & 3) * CHROMA_STRIDE;
        uint2 v;
        if (filters) {
          v = filter_unit(s, f);
        } else {
          v = uint2(window[s >> 1], window[(s >> 1) + 1]);
        }
        store_unit(u < 4 ? data.dst_offset_u : data.dst_offset_v, data.uv_stride, uvx0, uvy0 + (u & 3), v);
      }
    }
  }
}
