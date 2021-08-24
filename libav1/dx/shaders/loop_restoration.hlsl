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

#pragma warning(disable : 3557)
#include "restoration.h"

#define MI_SIZE 4
#define MI_SIZE_LOG2 2
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
  int2 pad;
};
cbuffer PlaneRestorationConstBuffer : register(b1) { PlaneRestorationData pl[3]; };

cbuffer cb_loop_rest_data : register(b2) {
  int do_restoration;
  int plane_id;
}

#define WG_WIDTH 16
#define WG_HEIGHT 4

groupshared int input[WG_HEIGHT + 6][WG_WIDTH + 8];
groupshared int output[WG_HEIGHT][WG_WIDTH];
groupshared int intermediate[WG_HEIGHT + 6][WG_WIDTH];

groupshared int flt[2][WG_HEIGHT][WG_WIDTH];
groupshared int A[WG_HEIGHT + 2][WG_WIDTH + 2];
groupshared int B[WG_HEIGHT + 2][WG_WIDTH + 2];

#define get_loaded_source_sample(x, y) input[y + 3][x + 4]
int box_filter0(int w, int h, int r, int eps, int lx, int ly, int bit_depth) {
  uint n = (2 * r + 1) * (2 * r + 1);
  int id = ly * WG_WIDTH + lx;
  for (int idx = id; idx < (h + 2)*(w + 2); idx += WG_HEIGHT * WG_WIDTH) {
    int i = idx / (w + 2) - 1;
    int j = idx % (w + 2) - 1;
    uint a = 0;
    uint b = 0;
    for (int dy = -2; dy <= 2; dy++) {
      for (int dx = -2; dx <= 2; dx++) {
        uint c = get_loaded_source_sample(j + dx, i + dy);
        a += c * c;
        b += c;
      }
    }
    a = Round2(a, 2 * (bit_depth - 8));
    uint d = Round2(b, bit_depth - 8);
    uint p = max(0, int(a * n - d * d));
    uint z = Round2(p * eps, SGRPROJ_MTABLE_BITS);  // p*s in documentation
    z = min(z, 255);
    uint a2 = 0;
    if (z >= 255)
      a2 = 256;
    else if (z == 0)
      a2 = 1;
    else
      a2 = ((z << SGRPROJ_SGR_BITS) + (z >> 1)) / (z + 1);
    uint oneOverN = ((1 << SGRPROJ_RECIP_BITS) + (n >> 1)) / n;
    uint b2 = ((1 << SGRPROJ_SGR_BITS) - a2) * b * oneOverN;
    A[1 + i][1 + j] = a2;
    B[1 + i][1 + j] = Round2(b2, SGRPROJ_RECIP_BITS);
  }
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

int box_filter1(int w, int h, int r, int eps, int lx, int ly, int bit_depth) {
  uint n = (2 * r + 1) * (2 * r + 1);
  int id = ly * WG_WIDTH + lx;
  for (int idx = id; idx < (h + 2)*(w + 2); idx += WG_HEIGHT * WG_WIDTH) {
    int i = idx / (w + 2) - 1;
    int j = idx % (w + 2) - 1;
    uint a = 0;
    uint b = 0;
    for (int dy = -1; dy <= 1; dy++) {
      for (int dx = -1; dx <= 1; dx++) {
        uint c = get_loaded_source_sample(j + dx, i + dy);
        a += c * c;
        b += c;
      }
    }
    a = Round2(a, 2 * (bit_depth - 8));
    uint d = Round2(b, bit_depth - 8);
    uint p = max(0, int(a * n - d * d));
    uint z = Round2(p * eps, SGRPROJ_MTABLE_BITS);  // p*s in documentation
    z = min(z, 255);
    uint a2 = 0;
    if (z >= 255)
      a2 = 256;
    else if (z == 0)
      a2 = 1;
    else
      a2 = ((z << SGRPROJ_SGR_BITS) + (z >> 1)) / (z + 1);
    uint oneOverN = ((1 << SGRPROJ_RECIP_BITS) + (n >> 1)) / n;
    uint b2 = ((1 << SGRPROJ_SGR_BITS) - a2) * b * oneOverN;
    A[1 + i][1 + j] = a2;
    B[1 + i][1 + j] = Round2(b2, SGRPROJ_RECIP_BITS);
  }
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

[numthreads(WG_WIDTH, WG_HEIGHT, 1)] void main(uint3 thread
                                               : SV_DispatchThreadID) {
  const int gx = thread.x & (~(WG_WIDTH - 1));
  const int gy = thread.y & (~(WG_HEIGHT - 1));
  const int lx = thread.x & (WG_WIDTH - 1);
  const int ly = thread.y & (WG_HEIGHT - 1);

  const int subsampling = pl[plane_id].subsampling;
  const int bit_depth = pl[plane_id].bit_depth;

  // Load block
  int stripe_id = uint((gy << subsampling) + 8) / STRIPE_SIZE;
  int StripeStartY = ((stripe_id * STRIPE_SIZE) - 8) >> subsampling;
  int StripeEndY = StripeStartY + (STRIPE_SIZE >> subsampling) - 1;

  PlaneInfo plane = pl[plane_id].plane;
  int block_offset = 0;

  const int dst_offset = pl[plane_id].dst_offset;

  UnitsInfo units = pl[plane_id].units;
  int unitCol = min(units.Cols - 1, uint(gx + 1) / units.Size);
  int unitRow = min(units.Rows - 1, uint((gy + 1 + (8 >> subsampling))) / units.Size);
  int unitId = unitRow * units.Stride + unitCol + pl[plane_id].Lr_buffer_offset;
  int4 rType = int4(RESTORE_NONE, 0, 0, 0);
  if (do_restoration) rType = LrTypeSgr.Load4(unitId * 16);

  if (rType.x == RESTORE_NONE || !do_restoration) {
    if ((thread.x & 3) == 0) {
      if (pl[plane_id].hbd) {
        uint2 input_char = src.Load2(plane.offset + plane.stride * thread.y + (thread.x << 1));
        dst.Store2(dst_offset + plane.stride * thread.y + (thread.x << 1), input_char);
      } else {
        uint input_char = src.Load(plane.offset + plane.stride * thread.y + thread.x);
        dst.Store(dst_offset + plane.stride * thread.y + thread.x, input_char);
      }
    }
    return;
  }

  for (int y = ly; y < WG_HEIGHT + 6; y += WG_HEIGHT)
    for (int x = lx; x < WG_WIDTH / 4 + 2; x += WG_WIDTH) {
      int c_y = clamp(y + gy - 3, StripeStartY - 2, StripeEndY + 2);
      c_y = clamp(c_y, 0, plane.height - 1);
      int nc_x = (x << 2) + gx - 4;
      int c_x = clamp(nc_x, 0, plane.width - 1) & (~3);
      int offset = (c_y < StripeStartY || c_y > StripeEndY) ? pl[plane_id].pp_offset : plane.offset;

      if (pl[plane_id].hbd) {
        int shift_max = nc_x < 0 ? 0 : (plane.width - c_x - 1) * 16;
        int shift_min = (nc_x - c_x) >= 4 ? shift_max : 0;
        block_offset = c_y * plane.stride + (c_x << 1);

        uint2 input_char = src.Load2(offset + block_offset);
        uint4 shift = uint4(clamp(0, shift_min, shift_max), clamp(16, shift_min, shift_max),
                            clamp(32, shift_min, shift_max), clamp(48, shift_min, shift_max));
        input[y][x * 4 + 0] = (shift.x > 16 ? (input_char.y >> (shift.x - 32)) : (input_char.x >> shift.x)) & 0xffff;
        input[y][x * 4 + 1] = (shift.y > 16 ? (input_char.y >> (shift.y - 32)) : (input_char.x >> shift.y)) & 0xffff;
        input[y][x * 4 + 2] = (shift.z > 16 ? (input_char.y >> (shift.z - 32)) : (input_char.x >> shift.z)) & 0xffff;
        input[y][x * 4 + 3] = (shift.w > 16 ? (input_char.y >> (shift.w - 32)) : (input_char.x >> shift.w)) & 0xffff;
      } else {
        int shift_max = nc_x < 0 ? 0 : (plane.width - c_x - 1) * 8;
        int shift_min = (nc_x - c_x) >= 4 ? shift_max : 0;
        block_offset = c_y * plane.stride + c_x;
        uint input_char = src.Load(offset + block_offset);
        input[y][x * 4 + 0] = (input_char >> clamp(0, shift_min, shift_max)) & 255;
        input[y][x * 4 + 1] = (input_char >> clamp(8, shift_min, shift_max)) & 255;
        input[y][x * 4 + 2] = (input_char >> clamp(16, shift_min, shift_max)) & 255;
        input[y][x * 4 + 3] = (input_char >> clamp(24, shift_min, shift_max)) & 255;
      }
    }

  GroupMemoryBarrier();

  if (rType.x == RESTORE_WIENER) {
    int limit = (1 << (bit_depth + 1 + FILTER_BITS - InterRound0)) - 1;

    int Lr_offset = unitId * 64;
    int4 hfilter0 = LrWiener.Load4(Lr_offset + 0);
    int4 hfilter1 = LrWiener.Load4(Lr_offset + 16);

    for (int r = ly; r < WG_HEIGHT + 6; r += WG_HEIGHT) {
      int s = (input[r][lx + 4] << 7) + (1 << (bit_depth + 7 - 1));

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
    output[ly][lx] = clamp(v, 0, (1 << bit_depth) - 1);  // input[ly][lx + 4];//
    if (lx < WG_WIDTH / 4) {
      if (pl[plane_id].hbd) {
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

    int r0 = data.Sgr_Params[rType.w].x;
    int r1 = data.Sgr_Params[rType.w].y;
    int eps0 = data.Sgr_Params[rType.w].z;
    int eps1 = data.Sgr_Params[rType.w].w;

    int u = input[ly + 3][lx + 4] << SGRPROJ_RST_BITS;
    int v = w1 * u;
    if (r0) {
        v += w0 * box_filter0(WG_WIDTH, WG_HEIGHT, r0, eps0, lx, ly, bit_depth);
    } else {
        v += w0 * u;
    }
    if (r1) {
        v += w2 * box_filter1(WG_WIDTH, WG_HEIGHT, r1, eps1, lx, ly, bit_depth);
    } else {
        v += w2 * u;
    }
    
    int s = Round2(v, (SGRPROJ_RST_BITS + SGRPROJ_PRJ_BITS));
    output[ly][lx] = clamp(s, 0, (1 << bit_depth) - 1);
    if (lx < WG_WIDTH / 4) {
      if (pl[plane_id].hbd) {
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
