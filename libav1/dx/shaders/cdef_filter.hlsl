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

#define CDEF_BLOCK_WIDTH 32
#define CDEF_BLOCK_HEIGHT 32
#define CDEF_UV_BLOCK_WIDTH 16
#define CDEF_UV_BLOCK_HEIGHT 16

#define CDEF_WIDTH 64
#define CDEF_HEIGHT 64

#define CDEF_VERY_LARGE (30000)

#define WG_WIDTH 4
#define WG_HEIGHT 4

groupshared int input[CDEF_BLOCK_HEIGHT + 18][CDEF_BLOCK_WIDTH + 8];
groupshared int output[CDEF_BLOCK_HEIGHT][CDEF_BLOCK_WIDTH];
groupshared int costs[8][WG_HEIGHT][WG_WIDTH];
groupshared int2 temp[WG_HEIGHT][WG_WIDTH];

// TODO: reorganize load (optimization)
void load_input(int4 plane, int gx, int gy, int llx, int lly, int llz) {
  uint id = ((llz * 4 + lly) * 4 + llx);
  int lx = id % (CDEF_BLOCK_WIDTH / 4 + 2);
  int ly = id / (CDEF_BLOCK_WIDTH / 4 + 2);

  plane.z >>= 2;
  for (int y = gy - 3 + ly; y < gy - 3 + CDEF_BLOCK_HEIGHT + 6; y += WG_HEIGHT) {
    const int gx4 = gx >> 2;
    for (int x = gx4 - 1 + lx; x < gx4 - 1 + CDEF_BLOCK_WIDTH / 4 + 2; x += WG_WIDTH) {
      int is_clamp = (x < 0) || (y < 0) || (x >= plane.z) || (y >= plane.w);
      int4 in_4 = {CDEF_VERY_LARGE, CDEF_VERY_LARGE, CDEF_VERY_LARGE, CDEF_VERY_LARGE};
      if (!is_clamp) {
        uint input_char = dst.Load(plane.y + y * plane.x + x * 4);
        in_4.x = (input_char >> 0) & 255;
        in_4.y = (input_char >> 8) & 255;
        in_4.z = (input_char >> 16) & 255;
        in_4.w = (input_char >> 24) & 255;
      }
      input[y - (gy - 3)][(x - (gx4 - 1)) * 4 + 0] = in_4.x;
      input[y - (gy - 3)][(x - (gx4 - 1)) * 4 + 1] = in_4.y;
      input[y - (gy - 3)][(x - (gx4 - 1)) * 4 + 2] = in_4.z;
      input[y - (gy - 3)][(x - (gx4 - 1)) * 4 + 3] = in_4.w;
    }
  }
}
void load_input_hbd(int4 plane, int gx, int gy, int llx, int lly, int llz) {
  uint id = ((llz * 4 + lly) * 4 + llx);
  int lx = id % (CDEF_BLOCK_WIDTH / 4 + 2);
  int ly = id / (CDEF_BLOCK_WIDTH / 4 + 2);

  plane.z >>= 2;
  for (int y = gy - 3 + ly; y < gy - 3 + CDEF_BLOCK_HEIGHT + 6; y += WG_HEIGHT) {
    const int gx4 = gx >> 2;
    for (int x = gx4 - 1 + lx; x < gx4 - 1 + CDEF_BLOCK_WIDTH / 4 + 2; x += WG_WIDTH) {
      int is_clamp = (x < 0) || (y < 0) || (x >= plane.z) || (y >= plane.w);
      int4 in_4 = {CDEF_VERY_LARGE, CDEF_VERY_LARGE, CDEF_VERY_LARGE, CDEF_VERY_LARGE};
      if (!is_clamp) {
        uint2 input_char = dst.Load2(plane.y + y * plane.x + (x << 3));
        in_4.x = (input_char.x >> 0) & 0x03ff;
        in_4.y = (input_char.x >> 16) & 0x03ff;
        in_4.z = (input_char.y >> 0) & 0x03ff;
        in_4.w = (input_char.y >> 16) & 0x03ff;
      }
      input[y - (gy - 3)][(x - (gx4 - 1)) * 4 + 0] = in_4.x;
      input[y - (gy - 3)][(x - (gx4 - 1)) * 4 + 1] = in_4.y;
      input[y - (gy - 3)][(x - (gx4 - 1)) * 4 + 2] = in_4.z;
      input[y - (gy - 3)][(x - (gx4 - 1)) * 4 + 3] = in_4.w;
    }
  }
}

// TODO: reorganize load (optimization)
void load_uv_input(int4 plane, int gx, int gy, int llx, int lly, int llz, int pid) {
  uint id = ((llz * 4 + lly) * 4 + llx);
  int lx = id % (CDEF_UV_BLOCK_WIDTH / 4 + 2);
  int ly = id / (CDEF_UV_BLOCK_WIDTH / 4 + 2);
  plane.z >>= 2;
  for (int y = gy - 3 + ly; y < gy - 3 + CDEF_UV_BLOCK_HEIGHT + 6; y += WG_HEIGHT) {
    const int gx4 = gx >> 2;
    for (int x = gx4 - 1 + lx; x < gx4 - 1 + CDEF_UV_BLOCK_WIDTH / 4 + 2; x += WG_WIDTH) {
      int is_clamp = (x < 0) || (y < 0) || (x >= plane.z) || (y >= plane.w);
      int4 in_4 = {CDEF_VERY_LARGE, CDEF_VERY_LARGE, CDEF_VERY_LARGE, CDEF_VERY_LARGE};
      if (!is_clamp) {
        uint input_char = dst.Load(plane.y + y * plane.x + x * 4);
        in_4.x = (input_char >> 0) & 255;
        in_4.y = (input_char >> 8) & 255;
        in_4.z = (input_char >> 16) & 255;
        in_4.w = (input_char >> 24) & 255;
      }
      input[y - (gy - 3) + (pid - 1) * 24][(x - (gx4 - 1)) * 4 + 0] = in_4.x;
      input[y - (gy - 3) + (pid - 1) * 24][(x - (gx4 - 1)) * 4 + 1] = in_4.y;
      input[y - (gy - 3) + (pid - 1) * 24][(x - (gx4 - 1)) * 4 + 2] = in_4.z;
      input[y - (gy - 3) + (pid - 1) * 24][(x - (gx4 - 1)) * 4 + 3] = in_4.w;
    }
  }
}
void load_uv_input_hbd(int4 plane, int gx, int gy, int llx, int lly, int llz, int pid) {
  uint id = ((llz * 4 + lly) * 4 + llx);
  int lx = id % (CDEF_UV_BLOCK_WIDTH / 4 + 2);
  int ly = id / (CDEF_UV_BLOCK_WIDTH / 4 + 2);
  plane.z >>= 2;
  for (int y = gy - 3 + ly; y < gy - 3 + CDEF_UV_BLOCK_HEIGHT + 6; y += WG_HEIGHT) {
    const int gx4 = gx >> 2;
    for (int x = gx4 - 1 + lx; x < gx4 - 1 + CDEF_UV_BLOCK_WIDTH / 4 + 2; x += WG_WIDTH) {
      int is_clamp = (x < 0) || (y < 0) || (x >= plane.z) || (y >= plane.w);
      int4 in_4 = {CDEF_VERY_LARGE, CDEF_VERY_LARGE, CDEF_VERY_LARGE, CDEF_VERY_LARGE};
      if (!is_clamp) {
        uint2 input_char = dst.Load2(plane.y + y * plane.x + (x << 3));
        in_4.x = (input_char.x >> 0) & 0x03ff;
        in_4.y = (input_char.x >> 16) & 0x03ff;
        in_4.z = (input_char.y >> 0) & 0x03ff;
        in_4.w = (input_char.y >> 16) & 0x03ff;
      }
      input[y - (gy - 3) + (pid - 1) * 24][(x - (gx4 - 1)) * 4 + 0] = in_4.x;
      input[y - (gy - 3) + (pid - 1) * 24][(x - (gx4 - 1)) * 4 + 1] = in_4.y;
      input[y - (gy - 3) + (pid - 1) * 24][(x - (gx4 - 1)) * 4 + 2] = in_4.z;
      input[y - (gy - 3) + (pid - 1) * 24][(x - (gx4 - 1)) * 4 + 3] = in_4.w;
    }
  }
}

int get_loaded_source_sample(int x, int y) { return input[y + 3][x + 4]; }

int2 get_block_dir(int x0, int y0, int lx, int ly, int lz, int coeff_shift) {
  int i;
  int z0 = lz;
  int cost[8] = {0, 0, 0, 0, 0, 0, 0, 0};

  const int div_table[] = {0, 840, 420, 280, 210, 168, 140, 120, 105};
  const int div_table_idx[8][8] = {{1, 2, 3, 4, 5, 6, 7, 8}, {2, 4, 6, 8, 8, 8, 8, 8}, {8, 8, 8, 8, 8, 8, 8, 8},
                                   {2, 4, 6, 8, 8, 8, 8, 8}, {1, 2, 3, 4, 5, 6, 7, 8}, {2, 4, 6, 8, 8, 8, 8, 8},
                                   {8, 8, 8, 8, 8, 8, 8, 8}, {2, 4, 6, 8, 8, 8, 8, 8}};
  const int4 prt_idx[8] = {{1, 1, 0, 0},  {1, 1, 0, 1},  {1, 0, 0, 0}, {1, -1, 0, 1},
                           {1, -1, 0, 0}, {-1, 1, 1, 0}, {0, 1, 0, 0}, {1, 1, 1, 0}};
  const int prt_idx_shift[8] = {0, 0, 0, 3, 7, 3, 0, 0};

  const int cost_prt_idx[8][8] = {{14, 13, 12, 11, 10, 9, 8, -1},
                                  {10, 9, 8, -1, -1, -1, -1, -1},
                                  {
                                      -1,
                                      -1,
                                      -1,
                                      -1,
                                      -1,
                                      -1,
                                      -1,
                                      -1,
                                  },
                                  {10, 9, 8, -1, -1, -1, -1, -1},
                                  {14, 13, 12, 11, 10, 9, 8, -1},
                                  {10, 9, 8, -1, -1, -1, -1, -1},
                                  {
                                      -1,
                                      -1,
                                      -1,
                                      -1,
                                      -1,
                                      -1,
                                      -1,
                                      -1,
                                  },
                                  {10, 9, 8, -1, -1, -1, -1, -1}};
  int partial[15] = {0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0};
  int best_cost = 0;
  int best_cost_2 = 0;
  int2 best_dir = {0, 0};
  for (i = 0; i < 8; i++) {
    int j;
    for (j = 0; j < 8; j++) {
      int x;
      x = (get_loaded_source_sample(x0 + j, y0 + i) >> coeff_shift) - 128;
      int4 idx = prt_idx[z0];
      partial[idx.x * (i >> idx.z) + idx.y * (j >> idx.w) + prt_idx_shift[z0]] += x;
    }
  }
  // for (i = 0; i < 8; i++)
  { costs[lz][ly][lx] = 0; }
  for (i = 0; i < 8; i++) {
    int pt1 = partial[i];
    int pt2 = cost_prt_idx[z0][i] >= 0 ? partial[cost_prt_idx[z0][i]] : 0;
    costs[lz][ly][lx] += (pt1 * pt1 + pt2 * pt2) * div_table[div_table_idx[z0][i]];
  }
  GroupMemoryBarrierWithGroupSync();
  for (i = 0; i < 8; i++) {
    cost[i] = costs[i][ly][lx];
  }
  for (i = 0; i < 8; i++) {
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

int2 get_block_dir_old(int x0, int y0, int coeff_shift) {
  int i;
  int j;
  int cost[8] = {0, 0, 0, 0, 0, 0, 0, 0};
  int partial[8][15] = {{0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0}, {0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0},
                        {0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0}, {0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0},
                        {0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0}, {0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0},
                        {0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0}, {0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0}};
  const int div_table[] = {0, 840, 420, 280, 210, 168, 140, 120, 105};

  int best_cost = 0;
  int best_cost_2 = 0;
  int2 best_dir = {0, 0};
  for (i = 0; i < 8; i++) {
    for (j = 0; j < 8; j++) {
      int x;
      x = (get_loaded_source_sample(x0 + j, y0 + i) >> coeff_shift) - 128;
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
  for (i = 0; i < 8; i++) {
    cost[2] += partial[2][i] * partial[2][i];
    cost[6] += partial[6][i] * partial[6][i];
  }
  cost[2] *= div_table[8];
  cost[6] *= div_table[8];
  for (i = 0; i < 7; i++) {
    cost[0] += (partial[0][i] * partial[0][i] + partial[0][14 - i] * partial[0][14 - i]) * div_table[i + 1];
    cost[4] += (partial[4][i] * partial[4][i] + partial[4][14 - i] * partial[4][14 - i]) * div_table[i + 1];
  }
  cost[0] += partial[0][7] * partial[0][7] * div_table[8];
  cost[4] += partial[4][7] * partial[4][7] * div_table[8];
  for (i = 1; i < 8; i += 2) {
    for (j = 0; j < 4 + 1; j++) {
      cost[i] += partial[i][3 + j] * partial[i][3 + j];
    }
    cost[i] *= div_table[8];
    for (j = 0; j < 4 - 1; j++) {
      cost[i] += (partial[i][j] * partial[i][j] + partial[i][10 - j] * partial[i][10 - j]) * div_table[2 * j + 2];
    }
  }
  for (i = 0; i < 8; i++) {
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

#define MAX_SB_SIZE_LOG2 7
#define ALIGN_POWER_OF_TWO(value, n) (((value) + ((1 << (n)) - 1)) & ~((1 << (n)) - 1))
#define CDEF_VBORDER (3)
#define CDEF_HBORDER (8)
#define CDEF_BSTRIDE ALIGN_POWER_OF_TWO((1 << MAX_SB_SIZE_LOG2) + 2 * CDEF_HBORDER, 3)

int get_msb(unsigned int n) {
  int log = 0;
  unsigned int value = n;
  int i;

  for (i = 4; i >= 0; --i) {
    const int shift = (1 << i);
    const unsigned int x = value >> shift;
    if (x != 0) {
      value = x;
      log += shift;
    }
  }
  return log;
}

int constrain(int diff, int threshold, int shift) {
  return sign(diff) * min(abs(diff), max(0, threshold - (abs(diff) >> shift)));
}

int adjust_strength(int strength, int var) {
  const int i = var >> 6 ? min(get_msb(var >> 6), 12) : 0;
  return var ? (strength * (4 + i) + 8) >> 4 : 0;
}

[numthreads(WG_WIDTH, WG_HEIGHT, 8)] void main(uint3 Gid
                                               : SV_GroupID, uint3 DTid
                                               : SV_DispatchThreadID, uint3 GTid
                                               : SV_GroupThreadID, uint GI
                                               : SV_GroupIndex) {
  int gx = Gid.x * CDEF_BLOCK_WIDTH;
  int gy = Gid.y * CDEF_BLOCK_HEIGHT;
  int guvx = Gid.x * CDEF_UV_BLOCK_WIDTH;
  int guvy = Gid.y * CDEF_UV_BLOCK_HEIGHT;
  int lx = GTid.x;
  int ly = GTid.y;
  int lz = GTid.z;
  int2 dir_var;
  int bit_depth = data.bit_depth;
  int coeff_shift = bit_depth - 8;

  int skip = skips.Load(((Gid.y * WG_HEIGHT + ly) * data.skips_stride + Gid.x * WG_WIDTH + lx) * 4);
  if (data.hbd) {
    load_input_hbd(data.pl, gx, gy, lx, ly, lz);
  } else {
    load_input(data.pl, gx, gy, lx, ly, lz);
  }
  GroupMemoryBarrierWithGroupSync();

  if (lz == 0 && !skip) {
    temp[ly][lx] = get_block_dir_old(lx * 8, ly * 8, coeff_shift);
  }
  int index = cdef_index.Load(((Gid.y >> 1) * data.index_stride + (Gid.x >> 1)) * 4);
  uint strength = 0;
  if (index >= 0 && index < 16) {
    strength = data.cdef_strength[index].x;
  }
  int t = skip ? 0 : strength / 4;
  int s = skip ? 0 : strength % 4;
  s += s == 3;

  GroupMemoryBarrierWithGroupSync();
  dir_var = temp[ly][lx];

  const int damping = data.pri_damping + coeff_shift;
  if (!skip) {
    const int pri_taps[2][2] = {{4, 2}, {3, 3}};
    const int sec_taps[2][2] = {{2, 1}, {2, 1}};

    const int x0 = lx * 8;
    const int y0 = ly * 8;
    const int z0 = lz;
    const int pri_strength = adjust_strength(t << coeff_shift, dir_var.y);
    const int sec_strength = s << coeff_shift;
    const int dir = t ? dir_var.x : 0;
    int i, j, k;
    const int p_t = (pri_strength >> coeff_shift) & 1;
    const int s_t = (pri_strength >> coeff_shift) & 1;
    const int pri_msb = get_msb(pri_strength);
    const int sec_msb = get_msb(sec_strength);
    const int pri_shift = max(0, damping - pri_msb);
    const int sec_shift = max(0, damping - sec_msb);

    // for (i = 0; i < 8; i++) {
    {
      i = z0;
      for (j = 0; j < 8; j++) {
        int sum = 0;
        int y;
        int x = get_loaded_source_sample(x0 + j, y0 + i);
        int mmax = x;
        int mmin = x;
        for (k = 0; k < 2; k++) {
          if (pri_strength) {
            int2 dir_pri = data.cdef_directions[dir][k].xy;
            int p0 = get_loaded_source_sample(x0 + j + dir_pri.x, y0 + i + dir_pri.y);
            int p1 = get_loaded_source_sample(x0 + j - dir_pri.x, y0 + i - dir_pri.y);
            sum += pri_taps[p_t][k] * constrain(p0 - x, pri_strength, pri_shift);
            sum += pri_taps[p_t][k] * constrain(p1 - x, pri_strength, pri_shift);

            // mmax = max(p0 & 0xff, mmax);
            // mmax = max(p1 & 0xff, mmax);
            // NOTE!: (CDEF_VERY_LARGE & 0xff) = 48, can we adjust CDEF_VERY_LARGE???
            if (p0 != CDEF_VERY_LARGE) mmax = max(p0, mmax);
            if (p1 != CDEF_VERY_LARGE) mmax = max(p1, mmax);
            mmin = min(p0, mmin);
            mmin = min(p1, mmin);
          }
          if (sec_strength) {
            int2 dir1_sec = data.cdef_directions[dir + 2][k].xy;
            int2 dir2_sec = data.cdef_directions[dir + 6][k].xy;
            int s0 = get_loaded_source_sample(x0 + j + dir1_sec.x, y0 + i + dir1_sec.y);
            int s1 = get_loaded_source_sample(x0 + j - dir1_sec.x, y0 + i - dir1_sec.y);
            int s2 = get_loaded_source_sample(x0 + j + dir2_sec.x, y0 + i + dir2_sec.y);
            int s3 = get_loaded_source_sample(x0 + j - dir2_sec.x, y0 + i - dir2_sec.y);
            // mmax = max(s0 & 0xff, mmax);
            // mmax = max(s1 & 0xff, mmax);
            // mmax = max(s2 & 0xff, mmax);
            // mmax = max(s3 & 0xff, mmax);
            // NOTE!: (CDEF_VERY_LARGE & 0xff) = 48, can we adjust CDEF_VERY_LARGE???
            if (s0 != CDEF_VERY_LARGE) mmax = max(s0, mmax);
            if (s1 != CDEF_VERY_LARGE) mmax = max(s1, mmax);
            if (s2 != CDEF_VERY_LARGE) mmax = max(s2, mmax);
            if (s3 != CDEF_VERY_LARGE) mmax = max(s3, mmax);
            mmin = min(s0, mmin);
            mmin = min(s1, mmin);
            mmin = min(s2, mmin);
            mmin = min(s3, mmin);
            sum += sec_taps[s_t][k] * constrain(s0 - x, sec_strength, sec_shift);
            sum += sec_taps[s_t][k] * constrain(s1 - x, sec_strength, sec_shift);
            sum += sec_taps[s_t][k] * constrain(s2 - x, sec_strength, sec_shift);
            sum += sec_taps[s_t][k] * constrain(s3 - x, sec_strength, sec_shift);
          }
        }
        y = clamp(x + ((8 + sum - (sum < 0)) >> 4), mmin, mmax);

        output[y0 + i][x0 + j] = y;
      }
    }

  } else {
    for (int j = 0; j < 8; j++) output[ly * 8 + lz][lx * 8 + j] = get_loaded_source_sample(lx * 8 + j, ly * 8 + lz);
  }
  GroupMemoryBarrierWithGroupSync();
  int dy = lz;
  {
    for (int dx = 0; dx < 2; dx++) {
      int4 out_sample;
      out_sample.x = output[ly * 8 + dy][lx * 8 + dx * 4 + 0];
      out_sample.y = output[ly * 8 + dy][lx * 8 + dx * 4 + 1];
      out_sample.z = output[ly * 8 + dy][lx * 8 + dx * 4 + 2];
      out_sample.w = output[ly * 8 + dy][lx * 8 + dx * 4 + 3];

      if (data.hbd) {
        dst.Store2(data.dst_offset_y + (gy + ly * 8 + dy) * data.pl.x + ((gx + lx * 8 + dx * 4) << 1),
                   uint2((out_sample.x << 0) | (out_sample.y << 16), (out_sample.z << 0) | (out_sample.w << 16)));
      } else {
        dst.Store(data.dst_offset_y + (gy + ly * 8 + dy) * data.pl.x + gx + lx * 8 + dx * 4,
                  (out_sample.x << 0) | (out_sample.y << 8) | (out_sample.z << 16) | (out_sample.w << 24));
      }
    }
  }
  // Chroma processing
  int pid = 1 + (lz >> 2);
  int llz = lz & 3;
  int4 plane = data.pl;
  plane.x = data.uv_stride;
  plane.y = pid == 1 ? data.uv_offset_u : data.uv_offset_v;
  plane.z >>= 1;
  plane.w >>= 1;
  GroupMemoryBarrierWithGroupSync();
  if (data.hbd) {
    load_uv_input_hbd(plane, guvx, guvy, lx, ly, llz, pid);
  } else {
    load_uv_input(plane, guvx, guvy, lx, ly, llz, pid);
  }
  GroupMemoryBarrierWithGroupSync();
  if (index >= 0 && index < 16) {
    strength = data.cdef_uv_strength[index].x;
  }
  t = skip ? 0 : strength / 4;
  s = skip ? 0 : strength % 4;
  s += s == 3;

  if (!skip) {
    const int pri_taps[2][2] = {{4, 2}, {3, 3}};
    const int sec_taps[2][2] = {{2, 1}, {2, 1}};

    const int x0 = lx * 4;
    const int y0 = ly * 4;
    const int z0 = llz;
    const int pri_strength = t << coeff_shift;
    const int sec_strength = s << coeff_shift;
    const int dir = pri_strength ? dir_var.x : 0;
    int i, j, k;
    const int p_t = (pri_strength >> coeff_shift) & 1;
    const int s_t = (pri_strength >> coeff_shift) & 1;
    const int pri_msb = get_msb(pri_strength);
    const int sec_msb = get_msb(sec_strength);
    const int pri_shift = max(0, damping - 1 - pri_msb);
    const int sec_shift = max(0, damping - 1 - sec_msb);

    // for (i = 0; i < 8; i++) {
    {
      i = z0;
      for (j = 0; j < 4; j++) {
        int sum = 0;
        int y;
        int x = get_loaded_source_sample(x0 + j, y0 + i + (pid - 1) * 24);
        int mmax = x;
        int mmin = x;
        for (k = 0; k < 2; k++) {
          if (pri_strength) {
            int2 dir_pri = data.cdef_directions[dir][k].xy;
            int p0 = get_loaded_source_sample(x0 + j + dir_pri.x, y0 + i + dir_pri.y + (pid - 1) * 24);
            int p1 = get_loaded_source_sample(x0 + j - dir_pri.x, y0 + i - dir_pri.y + (pid - 1) * 24);
            sum += pri_taps[p_t][k] * constrain(p0 - x, pri_strength, pri_shift);
            sum += pri_taps[p_t][k] * constrain(p1 - x, pri_strength, pri_shift);
            // mmax = max(p0 & 0xff, mmax);
            // mmax = max(p1 & 0xff, mmax);
            // NOTE!: (CDEF_VERY_LARGE & 0xff) = 48, can we adjust CDEF_VERY_LARGE???
            if (p0 != CDEF_VERY_LARGE) mmax = max(p0, mmax);
            if (p1 != CDEF_VERY_LARGE) mmax = max(p1, mmax);
            mmin = min(p0, mmin);
            mmin = min(p1, mmin);
          }
          if (sec_strength) {
            int2 dir1_sec = data.cdef_directions[dir + 2][k].xy;
            int2 dir2_sec = data.cdef_directions[dir + 6][k].xy;
            int s0 = get_loaded_source_sample(x0 + j + dir1_sec.x, y0 + i + dir1_sec.y + (pid - 1) * 24);
            int s1 = get_loaded_source_sample(x0 + j - dir1_sec.x, y0 + i - dir1_sec.y + (pid - 1) * 24);
            int s2 = get_loaded_source_sample(x0 + j + dir2_sec.x, y0 + i + dir2_sec.y + (pid - 1) * 24);
            int s3 = get_loaded_source_sample(x0 + j - dir2_sec.x, y0 + i - dir2_sec.y + (pid - 1) * 24);
            // mmax = max(s0 & 0xff, mmax);
            // mmax = max(s1 & 0xff, mmax);
            // mmax = max(s2 & 0xff, mmax);
            // mmax = max(s3 & 0xff, mmax);
            // NOTE!: (CDEF_VERY_LARGE & 0xff) = 48, can we adjust CDEF_VERY_LARGE???
            if (s0 != CDEF_VERY_LARGE) mmax = max(s0, mmax);
            if (s1 != CDEF_VERY_LARGE) mmax = max(s1, mmax);
            if (s2 != CDEF_VERY_LARGE) mmax = max(s2, mmax);
            if (s3 != CDEF_VERY_LARGE) mmax = max(s3, mmax);
            mmin = min(s0, mmin);
            mmin = min(s1, mmin);
            mmin = min(s2, mmin);
            mmin = min(s3, mmin);
            sum += sec_taps[s_t][k] * constrain(s0 - x, sec_strength, sec_shift);
            sum += sec_taps[s_t][k] * constrain(s1 - x, sec_strength, sec_shift);
            sum += sec_taps[s_t][k] * constrain(s2 - x, sec_strength, sec_shift);
            sum += sec_taps[s_t][k] * constrain(s3 - x, sec_strength, sec_shift);
          }
        }
        y = clamp(x + ((8 + sum - (sum < 0)) >> 4), mmin, mmax);
        output[y0 + i + (pid - 1) * 16][x0 + j] = y;
      }
    }

  } else {
    for (int j = 0; j < 4; j++)
      output[ly * 4 + llz + (pid - 1) * 16][lx * 4 + j] =
          get_loaded_source_sample(lx * 4 + j, ly * 4 + llz + (pid - 1) * 24);
  }
  GroupMemoryBarrierWithGroupSync();
  dy = llz;
  {
    int4 out_sample;
    out_sample.x = output[ly * 4 + dy + (pid - 1) * 16][lx * 4 + 0];
    out_sample.y = output[ly * 4 + dy + (pid - 1) * 16][lx * 4 + 1];
    out_sample.z = output[ly * 4 + dy + (pid - 1) * 16][lx * 4 + 2];
    out_sample.w = output[ly * 4 + dy + (pid - 1) * 16][lx * 4 + 3];
    const int dst_offset_uv = pid == 1 ? data.dst_offset_u : data.dst_offset_v;
    if (data.hbd) {
      dst.Store2(dst_offset_uv + (guvy + ly * 4 + dy) * data.uv_stride + ((guvx + lx * 4) << 1),
                 uint2((out_sample.x << 0) | (out_sample.y << 16), (out_sample.z << 0) | (out_sample.w << 16)));
    } else {
      dst.Store(dst_offset_uv + (guvy + ly * 4 + dy) * data.uv_stride + guvx + lx * 4,
                (out_sample.x << 0) | (out_sample.y << 8) | (out_sample.z << 16) | (out_sample.w << 24));
    }
  }
}
