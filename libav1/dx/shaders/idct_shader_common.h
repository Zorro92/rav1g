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
#pragma warning(disable : 4714)

#define IdctBlockSize 16

ByteAddressBuffer buf_blocks : register(t0);
RWByteAddressBuffer buf_dst : register(u0);
RWByteAddressBuffer buf_input : register(u1);

cbuffer cb_idct_frame_data : register(b1) {
  uint4 cb_planes[3];
  int cb_bitdepth;
};

cbuffer cb_idct_dispatch_data : register(b2) {
  uint cb_index_offset;
  uint cb_wicount;
};

int clamp_value(int value, int2 range) { return clamp(value, range.x, range.y); }


#define NewSqrt2Bits 12
#define NewSqrt2 5793
#define NewInvSqrt2 2896
#define CosBits 12
#define ScanShift 11
#define ScanMask 3
#define IdctType_H_Flip (1 << 15)
#define IdctType_V_Flip (1 << 16)
#define IdctType_H_Shift 17
#define IdctType_V_Shift 19
#define ClampMaxValue16 ((1 << 15) - 1)
#define ClampMinValue16 (-(1 << 15))

#define IDCT_GEN(N, COLS, ROWS, ROWS0, COEF_LOOP, SCALE_COEF, TRANSFORM_ROW, TRANSFORM_COL)                           \
  [numthreads(64, 1, 1)] void main(uint3 thread                                                                       \
                                   : SV_DispatchThreadID) {                                                           \
    /* A thread behind the last block of the dispatch does nothing, but it takes part in the group syncs: */          \
    /* a group sync has to be reached by all threads of the group. */                                                 \
    const bool active = thread.x < cb_wicount;                                                                        \
    const uint wi = thread.x & (N - 1);                                                                               \
    uint4 block = uint4(0, 0, 0, 0);                                                                                  \
    [branch] if (active) block = buf_blocks.Load4((cb_index_offset + thread.x / N) * IdctBlockSize);                  \
    const uint coef_count = block.x & 0x7ff;                                                                          \
    const uint scan_offset = ScanSize * ((block.x >> ScanShift) & ScanMask) + wi;                                     \
    const uint loc_mem_offset = COLS * ROWS * N * (thread.x & (64 - N));                                              \
    const uint input_offset = block.y + wi * 4;                                                                       \
    const int2 row_clamp = int2(-(1 << (cb_bitdepth + 7)), (1 << (cb_bitdepth + 7)) - 1);                             \
    uint i;                                                                                                           \
    if (active) for (i = 0; i < COEF_LOOP; ++i) {                                                                     \
      int4 coefs = int4(0, 0, 0, 0);                                                                                  \
      if ((wi + i * N) < coef_count) {                                                                                \
        const int addr = input_offset * 4 + i * N * 16;                                                               \
        coefs = (int4)buf_input.Load4(addr);                                                                          \
        buf_input.Store4(addr, uint4(0, 0, 0, 0));                                                                    \
        coefs.x = clamp_value(coefs.x, row_clamp);                                                                    \
        coefs.y = clamp_value(coefs.y, row_clamp);                                                                    \
        coefs.z = clamp_value(coefs.z, row_clamp);                                                                    \
        coefs.w = clamp_value(coefs.w, row_clamp);                                                                    \
        if (SCALE_COEF) {                                                                                             \
          coefs.x = round_shift(coefs.x * NewInvSqrt2, NewSqrt2Bits);                                                 \
          coefs.y = round_shift(coefs.y * NewInvSqrt2, NewSqrt2Bits);                                                 \
          coefs.z = round_shift(coefs.z * NewInvSqrt2, NewSqrt2Bits);                                                 \
          coefs.w = round_shift(coefs.w * NewInvSqrt2, NewSqrt2Bits);                                                 \
        }                                                                                                             \
      }                                                                                                               \
      shared_mem[loc_mem_offset + cb_scans[scan_offset + i * N].x] = coefs.x;                                         \
      shared_mem[loc_mem_offset + cb_scans[scan_offset + i * N].y] = coefs.y;                                         \
      shared_mem[loc_mem_offset + cb_scans[scan_offset + i * N].z] = coefs.z;                                         \
      shared_mem[loc_mem_offset + cb_scans[scan_offset + i * N].w] = coefs.w;                                         \
    }                                                                                                                 \
    /* The coefficients of a block are placed by all its threads, a row is transformed by one. The threads */         \
    /* of a group run in no particular order, so the three group syncs here are what makes one pass */                \
    /* complete before the next one reads it. (Upstream had plain group memory barriers, which make no */             \
    /* thread wait: that held only where the 64 threads of a group ran in lockstep, as one wave.) */                  \
    GroupMemoryBarrierWithGroupSync();                                                                                \
    const uint h_type = (block.x >> IdctType_H_Shift) & 3;                                                            \
    const uint loc_row_offset = /*(thread.x & 63) * N * ROWS0 * COLS;*/ loc_mem_offset + wi * ROWS0 * COLS * N;       \
    if (active) for (int row = 0; row < ROWS0; ++row) {                                                               \
      const uint row_offset = loc_row_offset + row * N;                                                               \
      TRANSFORM_ROW(N* COLS, h_type, row_offset, 1, row_offset, 1, IdctOutputShiftH, row_clamp);                      \
    }                                                                                                                 \
                                                                                                                      \
    GroupMemoryBarrierWithGroupSync();                                                                                \
                                                                                                                      \
    const int v_type = (block.x >> IdctType_V_Shift) & 3;                                                             \
    const int loc_out_offset = (block.x & IdctType_V_Flip) ? loc_mem_offset + wi * COLS + (N * ROWS - 1) * N * COLS   \
                                                           : loc_mem_offset + wi * COLS;                              \
    const int loc_out_stride = (block.x & IdctType_V_Flip) ? -N * COLS : N * COLS;                                    \
    const int2 col_clamp = int2(ClampMinValue16, ClampMaxValue16);                                                    \
    if (active) for (int c = 0; c < COLS; ++c) {                                                                      \
      const int in_offset = loc_mem_offset + wi * COLS + c;                                                           \
      const int out_offset = loc_out_offset + c;                                                                      \
      TRANSFORM_COL(N* ROWS, v_type, in_offset, (N * COLS), out_offset, loc_out_stride, IdctOutputShiftV, col_clamp); \
    }                                                                                                                 \
                                                                                                                      \
    GroupMemoryBarrierWithGroupSync();                                                                                \
                                                                                                                      \
    const uint plane = (block.x >> 21) & 3;                                                                           \
    const uint stride = cb_planes[plane].x;                                                                           \
    const int output_offset = cb_planes[plane].y + 8 * ((wi & (N * COLS / 4 - 1)) + (block.z & 0xffff)) +             \
                              (wi / (N * COLS / 4) + (block.z >> 16) * 4) * stride;                                   \
    const uint col = wi & (N * COLS / 4 - 1);                                                                         \
    const int local_offset = loc_mem_offset + (wi / (N * COLS / 4)) * (N * COLS) +                                    \
                             ((block.x & IdctType_H_Flip) ? N * COLS - 4 - 4 * col : 4 * col);                        \
    if (active) for (i = 0; i < N * COLS * ROWS / 4; ++i) {                                                           \
      int4 sm;                                                                                                        \
      sm.x = shared_mem[local_offset + 0 + i * (4 * N)];                                                              \
      sm.y = shared_mem[local_offset + 1 + i * (4 * N)];                                                              \
      sm.z = shared_mem[local_offset + 2 + i * (4 * N)];                                                              \
      sm.w = shared_mem[local_offset + 3 + i * (4 * N)];                                                              \
      if (block.x & IdctType_H_Flip) sm.xyzw = sm.wzyx;                                                               \
      buf_dst.Store2(output_offset + (4 / COLS) * i * stride,                                                         \
                     int2((sm.x & 0xffff) | (sm.y << 16), (sm.z & 0xffff) | (sm.w << 16)));                           \
    }                                                                                                                 \
  }

#define TRANSFORM_SELECT4(N, type, in_offset, in_stride, out_offset, out_stride, shift, range)                     \
  {                                                                                                                \
    if (type == 0) {                                                                                               \
      IDCT4(shared_mem, in_offset, in_stride, shared_mem, out_offset, out_stride, range, shift);                   \
    } else if (type == 1) {                                                                                        \
      IADST4(shared_mem, in_offset, in_stride, shared_mem, out_offset, out_stride, range, shift);                  \
    } else {                                                                                                       \
      IIDENTITY_N(N, NewSqrt2, NewSqrt2Bits, shared_mem, in_offset, in_stride, shared_mem, out_offset, out_stride, \
                  range, shift);                                                                                   \
    }                                                                                                              \
  }

#define TRANSFORM_SELECT8(N, type, in_offset, in_stride, out_offset, out_stride, shift, range)                  \
  {                                                                                                             \
    if (type == 0) {                                                                                            \
      IDCT8(shared_mem, in_offset, in_stride, shared_mem, out_offset, out_stride, range, shift);                \
    } else if (type == 1) {                                                                                     \
      IADST8(shared_mem, in_offset, in_stride, shared_mem, out_offset, out_stride, range, shift);               \
    } else {                                                                                                    \
      IIDENTITY_N(N, 2, 0, shared_mem, in_offset, in_stride, shared_mem, out_offset, out_stride, range, shift); \
    }                                                                                                           \
  }

#define TRANSFORM_SELECT16(N, type, in_offset, in_stride, out_offset, out_stride, shift, range)                        \
  {                                                                                                                    \
    if (type == 0) {                                                                                                   \
      IDCT16(shared_mem, in_offset, in_stride, shared_mem, out_offset, out_stride, range, shift);                      \
    } else if (type == 1) {                                                                                            \
      IADST16(shared_mem, in_offset, in_stride, shared_mem, out_offset, out_stride, range, shift);                     \
    } else {                                                                                                           \
      IIDENTITY_N(N, 2 * NewSqrt2, NewSqrt2Bits, shared_mem, in_offset, in_stride, shared_mem, out_offset, out_stride, \
                  range, shift);                                                                                       \
    }                                                                                                                  \
  }

#define TRANSFORM_SELECT32(N, type, in_offset, in_stride, out_offset, out_stride, shift, range)                 \
  {                                                                                                             \
    if (type == 0) {                                                                                            \
      IDCT32(shared_mem, in_offset, in_stride, shared_mem, out_offset, out_stride, range, shift);               \
    } else {                                                                                                    \
      IIDENTITY_N(N, 4, 0, shared_mem, in_offset, in_stride, shared_mem, out_offset, out_stride, range, shift); \
    }                                                                                                           \
  }

#define TRANSFORM_64_HALF(N, type, in_offset, in_stride, out_offset, out_stride, shift, range)    \
  {                                                                                               \
    if (wi < 32) {                                                                                \
      IDCT64(shared_mem, in_offset, in_stride, shared_mem, out_offset, out_stride, range, shift); \
    }                                                                                             \
  }

#define TRANSFORM_64(N, type, in_offset, in_stride, out_offset, out_stride, shift, range) \
  { IDCT64(shared_mem, in_offset, in_stride, shared_mem, out_offset, out_stride, range, shift); }

static const int cospi[] = {4096, 4095, 4091, 4085, 4076, 4065, 4052, 4036, 4017, 3996, 3973, 3948, 3920,
                            3889, 3857, 3822, 3784, 3745, 3703, 3659, 3612, 3564, 3513, 3461, 3406, 3349,
                            3290, 3229, 3166, 3102, 3035, 2967, 2896, 2824, 2751, 2675, 2598, 2520, 2440,
                            2359, 2276, 2191, 2106, 2019, 1931, 1842, 1751, 1660, 1567, 1474, 1380, 1285,
                            1189, 1092, 995,  897,  799,  700,  601,  501,  401,  301,  201,  101};

static const int sinpi[] = {0, 1321, 2482, 3344, 3803};

int round_shift(int value, int bit) {
  if (bit == 0) return value;
  return (int)((value + (1 << (bit - 1))) >> bit);
}

int half_btf(int w0, int in0, int w1, int in1, int bit) { return (w0 * in0 + w1 * in1 + (1 << (bit - 1))) >> bit; }

#define IDCT4(input, in_offset, in_stride, output, out_offset, out_stride, range, output_shift)                 \
  {                                                                                                             \
    int step[4];                                                                                                \
    int step1[4];                                                                                               \
    step[0] = clamp_value(input[in_offset + 0 * (in_stride)], range);                                           \
    step[1] = clamp_value(input[in_offset + 2 * (in_stride)], range);                                           \
    step[2] = clamp_value(input[in_offset + 1 * (in_stride)], range);                                           \
    step[3] = clamp_value(input[in_offset + 3 * (in_stride)], range);                                           \
    step1[0] = half_btf(cospi[32], step[0], cospi[32], step[1], CosBits);                                       \
    step1[1] = half_btf(cospi[32], step[0], -cospi[32], step[1], CosBits);                                      \
    step1[2] = half_btf(cospi[48], step[2], -cospi[16], step[3], CosBits);                                      \
    step1[3] = half_btf(cospi[16], step[2], cospi[48], step[3], CosBits);                                       \
    output[out_offset + 0 * (out_stride)] = round_shift(clamp_value(step1[0] + step1[3], range), output_shift); \
    output[out_offset + 1 * (out_stride)] = round_shift(clamp_value(step1[1] + step1[2], range), output_shift); \
    output[out_offset + 2 * (out_stride)] = round_shift(clamp_value(step1[1] - step1[2], range), output_shift); \
    output[out_offset + 3 * (out_stride)] = round_shift(clamp_value(step1[0] - step1[3], range), output_shift); \
  }

#define IADST4(input, in_offset, in_stride, output, out_offset, out_stride, range, output_shift) \
  {                                                                                              \
    int s0, s1, s2, s3, s4, s5, s6, s7;                                                          \
    int x0 = clamp_value(input[in_offset + 0 * (in_stride)], range);                             \
    int x1 = clamp_value(input[in_offset + 1 * (in_stride)], range);                             \
    int x2 = clamp_value(input[in_offset + 2 * (in_stride)], range);                             \
    int x3 = clamp_value(input[in_offset + 3 * (in_stride)], range);                             \
    s0 = sinpi[1] * x0;                                                                          \
    s1 = sinpi[2] * x0;                                                                          \
    s2 = sinpi[3] * x1;                                                                          \
    s3 = sinpi[4] * x2;                                                                          \
    s4 = sinpi[1] * x2;                                                                          \
    s5 = sinpi[2] * x3;                                                                          \
    s6 = sinpi[4] * x3;                                                                          \
    s7 = (x0 - x2) + x3;                                                                         \
    s0 = s0 + s3;                                                                                \
    s1 = s1 - s4;                                                                                \
    s3 = s2;                                                                                     \
    s2 = sinpi[3] * s7;                                                                          \
    s0 = s0 + s5;                                                                                \
    s1 = s1 - s6;                                                                                \
    x0 = s0 + s3;                                                                                \
    x1 = s1 + s3;                                                                                \
    x2 = s2;                                                                                     \
    x3 = s0 + s1;                                                                                \
    x3 = x3 - s3;                                                                                \
    output[out_offset + 0 * (out_stride)] = round_shift(round_shift(x0, CosBits), output_shift); \
    output[out_offset + 1 * (out_stride)] = round_shift(round_shift(x1, CosBits), output_shift); \
    output[out_offset + 2 * (out_stride)] = round_shift(round_shift(x2, CosBits), output_shift); \
    output[out_offset + 3 * (out_stride)] = round_shift(round_shift(x3, CosBits), output_shift); \
  }

#define IADST8(input, in_offset, in_stride, output, out_offset, out_stride, range, output_shift) \
  {                                                                                              \
    int step0[8];                                                                                \
    int step1[8];                                                                                \
    step0[0] = clamp_value(input[in_offset + 7 * (in_stride)], range);                           \
    step0[1] = clamp_value(input[in_offset + 0 * (in_stride)], range);                           \
    step0[2] = clamp_value(input[in_offset + 5 * (in_stride)], range);                           \
    step0[3] = clamp_value(input[in_offset + 2 * (in_stride)], range);                           \
    step0[4] = clamp_value(input[in_offset + 3 * (in_stride)], range);                           \
    step0[5] = clamp_value(input[in_offset + 4 * (in_stride)], range);                           \
    step0[6] = clamp_value(input[in_offset + 1 * (in_stride)], range);                           \
    step0[7] = clamp_value(input[in_offset + 6 * (in_stride)], range);                           \
    step1[0] = half_btf(cospi[4], step0[0], cospi[60], step0[1], CosBits);                       \
    step1[1] = half_btf(cospi[60], step0[0], -cospi[4], step0[1], CosBits);                      \
    step1[2] = half_btf(cospi[20], step0[2], cospi[44], step0[3], CosBits);                      \
    step1[3] = half_btf(cospi[44], step0[2], -cospi[20], step0[3], CosBits);                     \
    step1[4] = half_btf(cospi[36], step0[4], cospi[28], step0[5], CosBits);                      \
    step1[5] = half_btf(cospi[28], step0[4], -cospi[36], step0[5], CosBits);                     \
    step1[6] = half_btf(cospi[52], step0[6], cospi[12], step0[7], CosBits);                      \
    step1[7] = half_btf(cospi[12], step0[6], -cospi[52], step0[7], CosBits);                     \
    step0[0] = clamp_value(step1[0] + step1[4], range);                                          \
    step0[1] = clamp_value(step1[1] + step1[5], range);                                          \
    step0[2] = clamp_value(step1[2] + step1[6], range);                                          \
    step0[3] = clamp_value(step1[3] + step1[7], range);                                          \
    step0[4] = clamp_value(step1[0] - step1[4], range);                                          \
    step0[5] = clamp_value(step1[1] - step1[5], range);                                          \
    step0[6] = clamp_value(step1[2] - step1[6], range);                                          \
    step0[7] = clamp_value(step1[3] - step1[7], range);                                          \
    step1[0] = step0[0];                                                                         \
    step1[1] = step0[1];                                                                         \
    step1[2] = step0[2];                                                                         \
    step1[3] = step0[3];                                                                         \
    step1[4] = half_btf(cospi[16], step0[4], cospi[48], step0[5], CosBits);                      \
    step1[5] = half_btf(cospi[48], step0[4], -cospi[16], step0[5], CosBits);                     \
    step1[6] = half_btf(-cospi[48], step0[6], cospi[16], step0[7], CosBits);                     \
    step1[7] = half_btf(cospi[16], step0[6], cospi[48], step0[7], CosBits);                      \
    step0[0] = clamp_value(step1[0] + step1[2], range);                                          \
    step0[1] = clamp_value(step1[1] + step1[3], range);                                          \
    step0[2] = clamp_value(step1[0] - step1[2], range);                                          \
    step0[3] = clamp_value(step1[1] - step1[3], range);                                          \
    step0[4] = clamp_value(step1[4] + step1[6], range);                                          \
    step0[5] = clamp_value(step1[5] + step1[7], range);                                          \
    step0[6] = clamp_value(step1[4] - step1[6], range);                                          \
    step0[7] = clamp_value(step1[5] - step1[7], range);                                          \
    step1[0] = step0[0];                                                                         \
    step1[1] = step0[1];                                                                         \
    step1[2] = half_btf(cospi[32], step0[2], cospi[32], step0[3], CosBits);                      \
    step1[3] = half_btf(cospi[32], step0[2], -cospi[32], step0[3], CosBits);                     \
    step1[4] = step0[4];                                                                         \
    step1[5] = step0[5];                                                                         \
    step1[6] = half_btf(cospi[32], step0[6], cospi[32], step0[7], CosBits);                      \
    step1[7] = half_btf(cospi[32], step0[6], -cospi[32], step0[7], CosBits);                     \
    output[out_offset + 0 * (out_stride)] = round_shift(step1[0], output_shift);                 \
    output[out_offset + 1 * (out_stride)] = round_shift(-step1[4], output_shift);                \
    output[out_offset + 2 * (out_stride)] = round_shift(step1[6], output_shift);                 \
    output[out_offset + 3 * (out_stride)] = round_shift(-step1[2], output_shift);                \
    output[out_offset + 4 * (out_stride)] = round_shift(step1[3], output_shift);                 \
    output[out_offset + 5 * (out_stride)] = round_shift(-step1[7], output_shift);                \
    output[out_offset + 6 * (out_stride)] = round_shift(step1[5], output_shift);                 \
    output[out_offset + 7 * (out_stride)] = round_shift(-step1[1], output_shift);                \
  }

#define IDCT8(input, in_offset, in_stride, output, out_offset, out_stride, range, output_shift)                 \
  {                                                                                                             \
    int step0[8];                                                                                               \
    int step1[8];                                                                                               \
    step0[0] = clamp_value(input[in_offset + 0 * (in_stride)], range);                                          \
    step0[1] = clamp_value(input[in_offset + 4 * (in_stride)], range);                                          \
    step0[2] = clamp_value(input[in_offset + 2 * (in_stride)], range);                                          \
    step0[3] = clamp_value(input[in_offset + 6 * (in_stride)], range);                                          \
    step0[4] = clamp_value(input[in_offset + 1 * (in_stride)], range);                                          \
    step0[5] = clamp_value(input[in_offset + 5 * (in_stride)], range);                                          \
    step0[6] = clamp_value(input[in_offset + 3 * (in_stride)], range);                                          \
    step0[7] = clamp_value(input[in_offset + 7 * (in_stride)], range);                                          \
    step1[0] = step0[0];                                                                                        \
    step1[1] = step0[1];                                                                                        \
    step1[2] = step0[2];                                                                                        \
    step1[3] = step0[3];                                                                                        \
    step1[4] = half_btf(cospi[56], step0[4], -cospi[8], step0[7], CosBits);                                     \
    step1[5] = half_btf(cospi[24], step0[5], -cospi[40], step0[6], CosBits);                                    \
    step1[6] = half_btf(cospi[40], step0[5], cospi[24], step0[6], CosBits);                                     \
    step1[7] = half_btf(cospi[8], step0[4], cospi[56], step0[7], CosBits);                                      \
    step0[0] = half_btf(cospi[32], step1[0], cospi[32], step1[1], CosBits);                                     \
    step0[1] = half_btf(cospi[32], step1[0], -cospi[32], step1[1], CosBits);                                    \
    step0[2] = half_btf(cospi[48], step1[2], -cospi[16], step1[3], CosBits);                                    \
    step0[3] = half_btf(cospi[16], step1[2], cospi[48], step1[3], CosBits);                                     \
    step0[4] = clamp_value(step1[4] + step1[5], range);                                                         \
    step0[5] = clamp_value(step1[4] - step1[5], range);                                                         \
    step0[6] = clamp_value(-step1[6] + step1[7], range);                                                        \
    step0[7] = clamp_value(step1[6] + step1[7], range);                                                         \
    step1[0] = clamp_value(step0[0] + step0[3], range);                                                         \
    step1[1] = clamp_value(step0[1] + step0[2], range);                                                         \
    step1[2] = clamp_value(step0[1] - step0[2], range);                                                         \
    step1[3] = clamp_value(step0[0] - step0[3], range);                                                         \
    step1[4] = step0[4];                                                                                        \
    step1[5] = half_btf(-cospi[32], step0[5], cospi[32], step0[6], CosBits);                                    \
    step1[6] = half_btf(cospi[32], step0[5], cospi[32], step0[6], CosBits);                                     \
    step1[7] = step0[7];                                                                                        \
    output[out_offset + 0 * (out_stride)] = round_shift(clamp_value(step1[0] + step1[7], range), output_shift); \
    output[out_offset + 1 * (out_stride)] = round_shift(clamp_value(step1[1] + step1[6], range), output_shift); \
    output[out_offset + 2 * (out_stride)] = round_shift(clamp_value(step1[2] + step1[5], range), output_shift); \
    output[out_offset + 3 * (out_stride)] = round_shift(clamp_value(step1[3] + step1[4], range), output_shift); \
    output[out_offset + 4 * (out_stride)] = round_shift(clamp_value(step1[3] - step1[4], range), output_shift); \
    output[out_offset + 5 * (out_stride)] = round_shift(clamp_value(step1[2] - step1[5], range), output_shift); \
    output[out_offset + 6 * (out_stride)] = round_shift(clamp_value(step1[1] - step1[6], range), output_shift); \
    output[out_offset + 7 * (out_stride)] = round_shift(clamp_value(step1[0] - step1[7], range), output_shift); \
  }

#define IDCT16(input, in_offset, in_stride, output, out_offset, out_stride, range, output_shift)                \
  {                                                                                                             \
    int step0[16];                                                                                              \
    int step1[16];                                                                                              \
    step0[0] = clamp_value(input[in_offset + (in_stride)*0], range);                                            \
    step0[1] = clamp_value(input[in_offset + (in_stride)*8], range);                                            \
    step0[2] = clamp_value(input[in_offset + (in_stride)*4], range);                                            \
    step0[3] = clamp_value(input[in_offset + (in_stride)*12], range);                                           \
    step0[4] = clamp_value(input[in_offset + (in_stride)*2], range);                                            \
    step0[5] = clamp_value(input[in_offset + (in_stride)*10], range);                                           \
    step0[6] = clamp_value(input[in_offset + (in_stride)*6], range);                                            \
    step0[7] = clamp_value(input[in_offset + (in_stride)*14], range);                                           \
    step0[8] = clamp_value(input[in_offset + (in_stride)*1], range);                                            \
    step0[9] = clamp_value(input[in_offset + (in_stride)*9], range);                                            \
    step0[10] = clamp_value(input[in_offset + (in_stride)*5], range);                                           \
    step0[11] = clamp_value(input[in_offset + (in_stride)*13], range);                                          \
    step0[12] = clamp_value(input[in_offset + (in_stride)*3], range);                                           \
    step0[13] = clamp_value(input[in_offset + (in_stride)*11], range);                                          \
    step0[14] = clamp_value(input[in_offset + (in_stride)*7], range);                                           \
    step0[15] = clamp_value(input[in_offset + (in_stride)*15], range);                                          \
    step1[0] = step0[0];                                                                                        \
    step1[1] = step0[1];                                                                                        \
    step1[2] = step0[2];                                                                                        \
    step1[3] = step0[3];                                                                                        \
    step1[4] = step0[4];                                                                                        \
    step1[5] = step0[5];                                                                                        \
    step1[6] = step0[6];                                                                                        \
    step1[7] = step0[7];                                                                                        \
    step1[8] = half_btf(cospi[60], step0[8], -cospi[4], step0[15], CosBits);                                    \
    step1[9] = half_btf(cospi[28], step0[9], -cospi[36], step0[14], CosBits);                                   \
    step1[10] = half_btf(cospi[44], step0[10], -cospi[20], step0[13], CosBits);                                 \
    step1[11] = half_btf(cospi[12], step0[11], -cospi[52], step0[12], CosBits);                                 \
    step1[12] = half_btf(cospi[52], step0[11], cospi[12], step0[12], CosBits);                                  \
    step1[13] = half_btf(cospi[20], step0[10], cospi[44], step0[13], CosBits);                                  \
    step1[14] = half_btf(cospi[36], step0[9], cospi[28], step0[14], CosBits);                                   \
    step1[15] = half_btf(cospi[4], step0[8], cospi[60], step0[15], CosBits);                                    \
    step0[0] = step1[0];                                                                                        \
    step0[1] = step1[1];                                                                                        \
    step0[2] = step1[2];                                                                                        \
    step0[3] = step1[3];                                                                                        \
    step0[4] = half_btf(cospi[56], step1[4], -cospi[8], step1[7], CosBits);                                     \
    step0[5] = half_btf(cospi[24], step1[5], -cospi[40], step1[6], CosBits);                                    \
    step0[6] = half_btf(cospi[40], step1[5], cospi[24], step1[6], CosBits);                                     \
    step0[7] = half_btf(cospi[8], step1[4], cospi[56], step1[7], CosBits);                                      \
    step0[8] = clamp_value(step1[8] + step1[9], range);                                                         \
    step0[9] = clamp_value(step1[8] - step1[9], range);                                                         \
    step0[10] = clamp_value(-step1[10] + step1[11], range);                                                     \
    step0[11] = clamp_value(step1[10] + step1[11], range);                                                      \
    step0[12] = clamp_value(step1[12] + step1[13], range);                                                      \
    step0[13] = clamp_value(step1[12] - step1[13], range);                                                      \
    step0[14] = clamp_value(-step1[14] + step1[15], range);                                                     \
    step0[15] = clamp_value(step1[14] + step1[15], range);                                                      \
    step1[0] = half_btf(cospi[32], step0[0], cospi[32], step0[1], CosBits);                                     \
    step1[1] = half_btf(cospi[32], step0[0], -cospi[32], step0[1], CosBits);                                    \
    step1[2] = half_btf(cospi[48], step0[2], -cospi[16], step0[3], CosBits);                                    \
    step1[3] = half_btf(cospi[16], step0[2], cospi[48], step0[3], CosBits);                                     \
    step1[4] = clamp_value(step0[4] + step0[5], range);                                                         \
    step1[5] = clamp_value(step0[4] - step0[5], range);                                                         \
    step1[6] = clamp_value(-step0[6] + step0[7], range);                                                        \
    step1[7] = clamp_value(step0[6] + step0[7], range);                                                         \
    step1[8] = step0[8];                                                                                        \
    step1[9] = half_btf(-cospi[16], step0[9], cospi[48], step0[14], CosBits);                                   \
    step1[10] = half_btf(-cospi[48], step0[10], -cospi[16], step0[13], CosBits);                                \
    step1[11] = step0[11];                                                                                      \
    step1[12] = step0[12];                                                                                      \
    step1[13] = half_btf(-cospi[16], step0[10], cospi[48], step0[13], CosBits);                                 \
    step1[14] = half_btf(cospi[48], step0[9], cospi[16], step0[14], CosBits);                                   \
    step1[15] = step0[15];                                                                                      \
    step0[0] = clamp_value(step1[0] + step1[3], range);                                                         \
    step0[1] = clamp_value(step1[1] + step1[2], range);                                                         \
    step0[2] = clamp_value(step1[1] - step1[2], range);                                                         \
    step0[3] = clamp_value(step1[0] - step1[3], range);                                                         \
    step0[4] = step1[4];                                                                                        \
    step0[5] = half_btf(-cospi[32], step1[5], cospi[32], step1[6], CosBits);                                    \
    step0[6] = half_btf(cospi[32], step1[5], cospi[32], step1[6], CosBits);                                     \
    step0[7] = step1[7];                                                                                        \
    step0[8] = clamp_value(step1[8] + step1[11], range);                                                        \
    step0[9] = clamp_value(step1[9] + step1[10], range);                                                        \
    step0[10] = clamp_value(step1[9] - step1[10], range);                                                       \
    step0[11] = clamp_value(step1[8] - step1[11], range);                                                       \
    step0[12] = clamp_value(-step1[12] + step1[15], range);                                                     \
    step0[13] = clamp_value(-step1[13] + step1[14], range);                                                     \
    step0[14] = clamp_value(step1[13] + step1[14], range);                                                      \
    step0[15] = clamp_value(step1[12] + step1[15], range);                                                      \
    step1[0] = clamp_value(step0[0] + step0[7], range);                                                         \
    step1[1] = clamp_value(step0[1] + step0[6], range);                                                         \
    step1[2] = clamp_value(step0[2] + step0[5], range);                                                         \
    step1[3] = clamp_value(step0[3] + step0[4], range);                                                         \
    step1[4] = clamp_value(step0[3] - step0[4], range);                                                         \
    step1[5] = clamp_value(step0[2] - step0[5], range);                                                         \
    step1[6] = clamp_value(step0[1] - step0[6], range);                                                         \
    step1[7] = clamp_value(step0[0] - step0[7], range);                                                         \
    step1[8] = step0[8];                                                                                        \
    step1[9] = step0[9];                                                                                        \
    step1[10] = half_btf(-cospi[32], step0[10], cospi[32], step0[13], CosBits);                                 \
    step1[11] = half_btf(-cospi[32], step0[11], cospi[32], step0[12], CosBits);                                 \
    step1[12] = half_btf(cospi[32], step0[11], cospi[32], step0[12], CosBits);                                  \
    step1[13] = half_btf(cospi[32], step0[10], cospi[32], step0[13], CosBits);                                  \
    step1[14] = step0[14];                                                                                      \
    step1[15] = step0[15];                                                                                      \
    output[out_offset + (out_stride)*0] = round_shift(clamp_value(step1[0] + step1[15], range), output_shift);  \
    output[out_offset + (out_stride)*1] = round_shift(clamp_value(step1[1] + step1[14], range), output_shift);  \
    output[out_offset + (out_stride)*2] = round_shift(clamp_value(step1[2] + step1[13], range), output_shift);  \
    output[out_offset + (out_stride)*3] = round_shift(clamp_value(step1[3] + step1[12], range), output_shift);  \
    output[out_offset + (out_stride)*4] = round_shift(clamp_value(step1[4] + step1[11], range), output_shift);  \
    output[out_offset + (out_stride)*5] = round_shift(clamp_value(step1[5] + step1[10], range), output_shift);  \
    output[out_offset + (out_stride)*6] = round_shift(clamp_value(step1[6] + step1[9], range), output_shift);   \
    output[out_offset + (out_stride)*7] = round_shift(clamp_value(step1[7] + step1[8], range), output_shift);   \
    output[out_offset + (out_stride)*8] = round_shift(clamp_value(step1[7] - step1[8], range), output_shift);   \
    output[out_offset + (out_stride)*9] = round_shift(clamp_value(step1[6] - step1[9], range), output_shift);   \
    output[out_offset + (out_stride)*10] = round_shift(clamp_value(step1[5] - step1[10], range), output_shift); \
    output[out_offset + (out_stride)*11] = round_shift(clamp_value(step1[4] - step1[11], range), output_shift); \
    output[out_offset + (out_stride)*12] = round_shift(clamp_value(step1[3] - step1[12], range), output_shift); \
    output[out_offset + (out_stride)*13] = round_shift(clamp_value(step1[2] - step1[13], range), output_shift); \
    output[out_offset + (out_stride)*14] = round_shift(clamp_value(step1[1] - step1[14], range), output_shift); \
    output[out_offset + (out_stride)*15] = round_shift(clamp_value(step1[0] - step1[15], range), output_shift); \
  }

#define IADST16(input, in_offset, in_stride, output, out_offset, out_stride, range, output_shift) \
  {                                                                                               \
    int step0[16];                                                                                \
    int step1[16];                                                                                \
    step0[0] = clamp_value(input[in_offset + (in_stride)*15], range);                             \
    step0[1] = clamp_value(input[in_offset + (in_stride)*0], range);                              \
    step0[2] = clamp_value(input[in_offset + (in_stride)*13], range);                             \
    step0[3] = clamp_value(input[in_offset + (in_stride)*2], range);                              \
    step0[4] = clamp_value(input[in_offset + (in_stride)*11], range);                             \
    step0[5] = clamp_value(input[in_offset + (in_stride)*4], range);                              \
    step0[6] = clamp_value(input[in_offset + (in_stride)*9], range);                              \
    step0[7] = clamp_value(input[in_offset + (in_stride)*6], range);                              \
    step0[8] = clamp_value(input[in_offset + (in_stride)*7], range);                              \
    step0[9] = clamp_value(input[in_offset + (in_stride)*8], range);                              \
    step0[10] = clamp_value(input[in_offset + (in_stride)*5], range);                             \
    step0[11] = clamp_value(input[in_offset + (in_stride)*10], range);                            \
    step0[12] = clamp_value(input[in_offset + (in_stride)*3], range);                             \
    step0[13] = clamp_value(input[in_offset + (in_stride)*12], range);                            \
    step0[14] = clamp_value(input[in_offset + (in_stride)*1], range);                             \
    step0[15] = clamp_value(input[in_offset + (in_stride)*14], range);                            \
    step1[0] = half_btf(cospi[2], step0[0], cospi[62], step0[1], CosBits);                        \
    step1[1] = half_btf(cospi[62], step0[0], -cospi[2], step0[1], CosBits);                       \
    step1[2] = half_btf(cospi[10], step0[2], cospi[54], step0[3], CosBits);                       \
    step1[3] = half_btf(cospi[54], step0[2], -cospi[10], step0[3], CosBits);                      \
    step1[4] = half_btf(cospi[18], step0[4], cospi[46], step0[5], CosBits);                       \
    step1[5] = half_btf(cospi[46], step0[4], -cospi[18], step0[5], CosBits);                      \
    step1[6] = half_btf(cospi[26], step0[6], cospi[38], step0[7], CosBits);                       \
    step1[7] = half_btf(cospi[38], step0[6], -cospi[26], step0[7], CosBits);                      \
    step1[8] = half_btf(cospi[34], step0[8], cospi[30], step0[9], CosBits);                       \
    step1[9] = half_btf(cospi[30], step0[8], -cospi[34], step0[9], CosBits);                      \
    step1[10] = half_btf(cospi[42], step0[10], cospi[22], step0[11], CosBits);                    \
    step1[11] = half_btf(cospi[22], step0[10], -cospi[42], step0[11], CosBits);                   \
    step1[12] = half_btf(cospi[50], step0[12], cospi[14], step0[13], CosBits);                    \
    step1[13] = half_btf(cospi[14], step0[12], -cospi[50], step0[13], CosBits);                   \
    step1[14] = half_btf(cospi[58], step0[14], cospi[6], step0[15], CosBits);                     \
    step1[15] = half_btf(cospi[6], step0[14], -cospi[58], step0[15], CosBits);                    \
    step0[0] = clamp_value(step1[0] + step1[8], range);                                           \
    step0[1] = clamp_value(step1[1] + step1[9], range);                                           \
    step0[2] = clamp_value(step1[2] + step1[10], range);                                          \
    step0[3] = clamp_value(step1[3] + step1[11], range);                                          \
    step0[4] = clamp_value(step1[4] + step1[12], range);                                          \
    step0[5] = clamp_value(step1[5] + step1[13], range);                                          \
    step0[6] = clamp_value(step1[6] + step1[14], range);                                          \
    step0[7] = clamp_value(step1[7] + step1[15], range);                                          \
    step0[8] = clamp_value(step1[0] - step1[8], range);                                           \
    step0[9] = clamp_value(step1[1] - step1[9], range);                                           \
    step0[10] = clamp_value(step1[2] - step1[10], range);                                         \
    step0[11] = clamp_value(step1[3] - step1[11], range);                                         \
    step0[12] = clamp_value(step1[4] - step1[12], range);                                         \
    step0[13] = clamp_value(step1[5] - step1[13], range);                                         \
    step0[14] = clamp_value(step1[6] - step1[14], range);                                         \
    step0[15] = clamp_value(step1[7] - step1[15], range);                                         \
    step1[0] = step0[0];                                                                          \
    step1[1] = step0[1];                                                                          \
    step1[2] = step0[2];                                                                          \
    step1[3] = step0[3];                                                                          \
    step1[4] = step0[4];                                                                          \
    step1[5] = step0[5];                                                                          \
    step1[6] = step0[6];                                                                          \
    step1[7] = step0[7];                                                                          \
    step1[8] = half_btf(cospi[8], step0[8], cospi[56], step0[9], CosBits);                        \
    step1[9] = half_btf(cospi[56], step0[8], -cospi[8], step0[9], CosBits);                       \
    step1[10] = half_btf(cospi[40], step0[10], cospi[24], step0[11], CosBits);                    \
    step1[11] = half_btf(cospi[24], step0[10], -cospi[40], step0[11], CosBits);                   \
    step1[12] = half_btf(-cospi[56], step0[12], cospi[8], step0[13], CosBits);                    \
    step1[13] = half_btf(cospi[8], step0[12], cospi[56], step0[13], CosBits);                     \
    step1[14] = half_btf(-cospi[24], step0[14], cospi[40], step0[15], CosBits);                   \
    step1[15] = half_btf(cospi[40], step0[14], cospi[24], step0[15], CosBits);                    \
    step0[0] = clamp_value(step1[0] + step1[4], range);                                           \
    step0[1] = clamp_value(step1[1] + step1[5], range);                                           \
    step0[2] = clamp_value(step1[2] + step1[6], range);                                           \
    step0[3] = clamp_value(step1[3] + step1[7], range);                                           \
    step0[4] = clamp_value(step1[0] - step1[4], range);                                           \
    step0[5] = clamp_value(step1[1] - step1[5], range);                                           \
    step0[6] = clamp_value(step1[2] - step1[6], range);                                           \
    step0[7] = clamp_value(step1[3] - step1[7], range);                                           \
    step0[8] = clamp_value(step1[8] + step1[12], range);                                          \
    step0[9] = clamp_value(step1[9] + step1[13], range);                                          \
    step0[10] = clamp_value(step1[10] + step1[14], range);                                        \
    step0[11] = clamp_value(step1[11] + step1[15], range);                                        \
    step0[12] = clamp_value(step1[8] - step1[12], range);                                         \
    step0[13] = clamp_value(step1[9] - step1[13], range);                                         \
    step0[14] = clamp_value(step1[10] - step1[14], range);                                        \
    step0[15] = clamp_value(step1[11] - step1[15], range);                                        \
    step1[0] = step0[0];                                                                          \
    step1[1] = step0[1];                                                                          \
    step1[2] = step0[2];                                                                          \
    step1[3] = step0[3];                                                                          \
    step1[4] = half_btf(cospi[16], step0[4], cospi[48], step0[5], CosBits);                       \
    step1[5] = half_btf(cospi[48], step0[4], -cospi[16], step0[5], CosBits);                      \
    step1[6] = half_btf(-cospi[48], step0[6], cospi[16], step0[7], CosBits);                      \
    step1[7] = half_btf(cospi[16], step0[6], cospi[48], step0[7], CosBits);                       \
    step1[8] = step0[8];                                                                          \
    step1[9] = step0[9];                                                                          \
    step1[10] = step0[10];                                                                        \
    step1[11] = step0[11];                                                                        \
    step1[12] = half_btf(cospi[16], step0[12], cospi[48], step0[13], CosBits);                    \
    step1[13] = half_btf(cospi[48], step0[12], -cospi[16], step0[13], CosBits);                   \
    step1[14] = half_btf(-cospi[48], step0[14], cospi[16], step0[15], CosBits);                   \
    step1[15] = half_btf(cospi[16], step0[14], cospi[48], step0[15], CosBits);                    \
    step0[0] = clamp_value(step1[0] + step1[2], range);                                           \
    step0[1] = clamp_value(step1[1] + step1[3], range);                                           \
    step0[2] = clamp_value(step1[0] - step1[2], range);                                           \
    step0[3] = clamp_value(step1[1] - step1[3], range);                                           \
    step0[4] = clamp_value(step1[4] + step1[6], range);                                           \
    step0[5] = clamp_value(step1[5] + step1[7], range);                                           \
    step0[6] = clamp_value(step1[4] - step1[6], range);                                           \
    step0[7] = clamp_value(step1[5] - step1[7], range);                                           \
    step0[8] = clamp_value(step1[8] + step1[10], range);                                          \
    step0[9] = clamp_value(step1[9] + step1[11], range);                                          \
    step0[10] = clamp_value(step1[8] - step1[10], range);                                         \
    step0[11] = clamp_value(step1[9] - step1[11], range);                                         \
    step0[12] = clamp_value(step1[12] + step1[14], range);                                        \
    step0[13] = clamp_value(step1[13] + step1[15], range);                                        \
    step0[14] = clamp_value(step1[12] - step1[14], range);                                        \
    step0[15] = clamp_value(step1[13] - step1[15], range);                                        \
    step1[0] = step0[0];                                                                          \
    step1[1] = step0[1];                                                                          \
    step1[2] = half_btf(cospi[32], step0[2], cospi[32], step0[3], CosBits);                       \
    step1[3] = half_btf(cospi[32], step0[2], -cospi[32], step0[3], CosBits);                      \
    step1[4] = step0[4];                                                                          \
    step1[5] = step0[5];                                                                          \
    step1[6] = half_btf(cospi[32], step0[6], cospi[32], step0[7], CosBits);                       \
    step1[7] = half_btf(cospi[32], step0[6], -cospi[32], step0[7], CosBits);                      \
    step1[8] = step0[8];                                                                          \
    step1[9] = step0[9];                                                                          \
    step1[10] = half_btf(cospi[32], step0[10], cospi[32], step0[11], CosBits);                    \
    step1[11] = half_btf(cospi[32], step0[10], -cospi[32], step0[11], CosBits);                   \
    step1[12] = step0[12];                                                                        \
    step1[13] = step0[13];                                                                        \
    step1[14] = half_btf(cospi[32], step0[14], cospi[32], step0[15], CosBits);                    \
    step1[15] = half_btf(cospi[32], step0[14], -cospi[32], step0[15], CosBits);                   \
    output[out_offset + (out_stride)*0] = round_shift(step1[0], output_shift);                    \
    output[out_offset + (out_stride)*1] = round_shift(-step1[8], output_shift);                   \
    output[out_offset + (out_stride)*2] = round_shift(step1[12], output_shift);                   \
    output[out_offset + (out_stride)*3] = round_shift(-step1[4], output_shift);                   \
    output[out_offset + (out_stride)*4] = round_shift(step1[6], output_shift);                    \
    output[out_offset + (out_stride)*5] = round_shift(-step1[14], output_shift);                  \
    output[out_offset + (out_stride)*6] = round_shift(step1[10], output_shift);                   \
    output[out_offset + (out_stride)*7] = round_shift(-step1[2], output_shift);                   \
    output[out_offset + (out_stride)*8] = round_shift(step1[3], output_shift);                    \
    output[out_offset + (out_stride)*9] = round_shift(-step1[11], output_shift);                  \
    output[out_offset + (out_stride)*10] = round_shift(step1[15], output_shift);                  \
    output[out_offset + (out_stride)*11] = round_shift(-step1[7], output_shift);                  \
    output[out_offset + (out_stride)*12] = round_shift(step1[5], output_shift);                   \
    output[out_offset + (out_stride)*13] = round_shift(-step1[13], output_shift);                 \
    output[out_offset + (out_stride)*14] = round_shift(step1[9], output_shift);                   \
    output[out_offset + (out_stride)*15] = round_shift(-step1[1], output_shift);                  \
  }

#define IDCT32(input, in_offset, in_stride, output, out_offset, out_stride, range, output_shift)                 \
  {                                                                                                              \
    int step0[32];                                                                                               \
    int step1[32];                                                                                               \
    step0[0] = clamp_value(input[in_offset + (in_stride)*0], range);                                             \
    step0[1] = clamp_value(input[in_offset + (in_stride)*16], range);                                            \
    step0[2] = clamp_value(input[in_offset + (in_stride)*8], range);                                             \
    step0[3] = clamp_value(input[in_offset + (in_stride)*24], range);                                            \
    step0[4] = clamp_value(input[in_offset + (in_stride)*4], range);                                             \
    step0[5] = clamp_value(input[in_offset + (in_stride)*20], range);                                            \
    step0[6] = clamp_value(input[in_offset + (in_stride)*12], range);                                            \
    step0[7] = clamp_value(input[in_offset + (in_stride)*28], range);                                            \
    step0[8] = clamp_value(input[in_offset + (in_stride)*2], range);                                             \
    step0[9] = clamp_value(input[in_offset + (in_stride)*18], range);                                            \
    step0[10] = clamp_value(input[in_offset + (in_stride)*10], range);                                           \
    step0[11] = clamp_value(input[in_offset + (in_stride)*26], range);                                           \
    step0[12] = clamp_value(input[in_offset + (in_stride)*6], range);                                            \
    step0[13] = clamp_value(input[in_offset + (in_stride)*22], range);                                           \
    step0[14] = clamp_value(input[in_offset + (in_stride)*14], range);                                           \
    step0[15] = clamp_value(input[in_offset + (in_stride)*30], range);                                           \
    step0[16] = clamp_value(input[in_offset + (in_stride)*1], range);                                            \
    step0[17] = clamp_value(input[in_offset + (in_stride)*17], range);                                           \
    step0[18] = clamp_value(input[in_offset + (in_stride)*9], range);                                            \
    step0[19] = clamp_value(input[in_offset + (in_stride)*25], range);                                           \
    step0[20] = clamp_value(input[in_offset + (in_stride)*5], range);                                            \
    step0[21] = clamp_value(input[in_offset + (in_stride)*21], range);                                           \
    step0[22] = clamp_value(input[in_offset + (in_stride)*13], range);                                           \
    step0[23] = clamp_value(input[in_offset + (in_stride)*29], range);                                           \
    step0[24] = clamp_value(input[in_offset + (in_stride)*3], range);                                            \
    step0[25] = clamp_value(input[in_offset + (in_stride)*19], range);                                           \
    step0[26] = clamp_value(input[in_offset + (in_stride)*11], range);                                           \
    step0[27] = clamp_value(input[in_offset + (in_stride)*27], range);                                           \
    step0[28] = clamp_value(input[in_offset + (in_stride)*7], range);                                            \
    step0[29] = clamp_value(input[in_offset + (in_stride)*23], range);                                           \
    step0[30] = clamp_value(input[in_offset + (in_stride)*15], range);                                           \
    step0[31] = clamp_value(input[in_offset + (in_stride)*31], range);                                           \
    step1[0] = step0[0];                                                                                         \
    step1[1] = step0[1];                                                                                         \
    step1[2] = step0[2];                                                                                         \
    step1[3] = step0[3];                                                                                         \
    step1[4] = step0[4];                                                                                         \
    step1[5] = step0[5];                                                                                         \
    step1[6] = step0[6];                                                                                         \
    step1[7] = step0[7];                                                                                         \
    step1[8] = step0[8];                                                                                         \
    step1[9] = step0[9];                                                                                         \
    step1[10] = step0[10];                                                                                       \
    step1[11] = step0[11];                                                                                       \
    step1[12] = step0[12];                                                                                       \
    step1[13] = step0[13];                                                                                       \
    step1[14] = step0[14];                                                                                       \
    step1[15] = step0[15];                                                                                       \
    step1[16] = half_btf(cospi[62], step0[16], -cospi[2], step0[31], CosBits);                                   \
    step1[17] = half_btf(cospi[30], step0[17], -cospi[34], step0[30], CosBits);                                  \
    step1[18] = half_btf(cospi[46], step0[18], -cospi[18], step0[29], CosBits);                                  \
    step1[19] = half_btf(cospi[14], step0[19], -cospi[50], step0[28], CosBits);                                  \
    step1[20] = half_btf(cospi[54], step0[20], -cospi[10], step0[27], CosBits);                                  \
    step1[21] = half_btf(cospi[22], step0[21], -cospi[42], step0[26], CosBits);                                  \
    step1[22] = half_btf(cospi[38], step0[22], -cospi[26], step0[25], CosBits);                                  \
    step1[23] = half_btf(cospi[6], step0[23], -cospi[58], step0[24], CosBits);                                   \
    step1[24] = half_btf(cospi[58], step0[23], cospi[6], step0[24], CosBits);                                    \
    step1[25] = half_btf(cospi[26], step0[22], cospi[38], step0[25], CosBits);                                   \
    step1[26] = half_btf(cospi[42], step0[21], cospi[22], step0[26], CosBits);                                   \
    step1[27] = half_btf(cospi[10], step0[20], cospi[54], step0[27], CosBits);                                   \
    step1[28] = half_btf(cospi[50], step0[19], cospi[14], step0[28], CosBits);                                   \
    step1[29] = half_btf(cospi[18], step0[18], cospi[46], step0[29], CosBits);                                   \
    step1[30] = half_btf(cospi[34], step0[17], cospi[30], step0[30], CosBits);                                   \
    step1[31] = half_btf(cospi[2], step0[16], cospi[62], step0[31], CosBits);                                    \
    step0[0] = step1[0];                                                                                         \
    step0[1] = step1[1];                                                                                         \
    step0[2] = step1[2];                                                                                         \
    step0[3] = step1[3];                                                                                         \
    step0[4] = step1[4];                                                                                         \
    step0[5] = step1[5];                                                                                         \
    step0[6] = step1[6];                                                                                         \
    step0[7] = step1[7];                                                                                         \
    step0[8] = half_btf(cospi[60], step1[8], -cospi[4], step1[15], CosBits);                                     \
    step0[9] = half_btf(cospi[28], step1[9], -cospi[36], step1[14], CosBits);                                    \
    step0[10] = half_btf(cospi[44], step1[10], -cospi[20], step1[13], CosBits);                                  \
    step0[11] = half_btf(cospi[12], step1[11], -cospi[52], step1[12], CosBits);                                  \
    step0[12] = half_btf(cospi[52], step1[11], cospi[12], step1[12], CosBits);                                   \
    step0[13] = half_btf(cospi[20], step1[10], cospi[44], step1[13], CosBits);                                   \
    step0[14] = half_btf(cospi[36], step1[9], cospi[28], step1[14], CosBits);                                    \
    step0[15] = half_btf(cospi[4], step1[8], cospi[60], step1[15], CosBits);                                     \
    step0[16] = clamp_value(step1[16] + step1[17], range);                                                       \
    step0[17] = clamp_value(step1[16] - step1[17], range);                                                       \
    step0[18] = clamp_value(-step1[18] + step1[19], range);                                                      \
    step0[19] = clamp_value(step1[18] + step1[19], range);                                                       \
    step0[20] = clamp_value(step1[20] + step1[21], range);                                                       \
    step0[21] = clamp_value(step1[20] - step1[21], range);                                                       \
    step0[22] = clamp_value(-step1[22] + step1[23], range);                                                      \
    step0[23] = clamp_value(step1[22] + step1[23], range);                                                       \
    step0[24] = clamp_value(step1[24] + step1[25], range);                                                       \
    step0[25] = clamp_value(step1[24] - step1[25], range);                                                       \
    step0[26] = clamp_value(-step1[26] + step1[27], range);                                                      \
    step0[27] = clamp_value(step1[26] + step1[27], range);                                                       \
    step0[28] = clamp_value(step1[28] + step1[29], range);                                                       \
    step0[29] = clamp_value(step1[28] - step1[29], range);                                                       \
    step0[30] = clamp_value(-step1[30] + step1[31], range);                                                      \
    step0[31] = clamp_value(step1[30] + step1[31], range);                                                       \
    step1[0] = step0[0];                                                                                         \
    step1[1] = step0[1];                                                                                         \
    step1[2] = step0[2];                                                                                         \
    step1[3] = step0[3];                                                                                         \
    step1[4] = half_btf(cospi[56], step0[4], -cospi[8], step0[7], CosBits);                                      \
    step1[5] = half_btf(cospi[24], step0[5], -cospi[40], step0[6], CosBits);                                     \
    step1[6] = half_btf(cospi[40], step0[5], cospi[24], step0[6], CosBits);                                      \
    step1[7] = half_btf(cospi[8], step0[4], cospi[56], step0[7], CosBits);                                       \
    step1[8] = clamp_value(step0[8] + step0[9], range);                                                          \
    step1[9] = clamp_value(step0[8] - step0[9], range);                                                          \
    step1[10] = clamp_value(-step0[10] + step0[11], range);                                                      \
    step1[11] = clamp_value(step0[10] + step0[11], range);                                                       \
    step1[12] = clamp_value(step0[12] + step0[13], range);                                                       \
    step1[13] = clamp_value(step0[12] - step0[13], range);                                                       \
    step1[14] = clamp_value(-step0[14] + step0[15], range);                                                      \
    step1[15] = clamp_value(step0[14] + step0[15], range);                                                       \
    step1[16] = step0[16];                                                                                       \
    step1[17] = half_btf(-cospi[8], step0[17], cospi[56], step0[30], CosBits);                                   \
    step1[18] = half_btf(-cospi[56], step0[18], -cospi[8], step0[29], CosBits);                                  \
    step1[19] = step0[19];                                                                                       \
    step1[20] = step0[20];                                                                                       \
    step1[21] = half_btf(-cospi[40], step0[21], cospi[24], step0[26], CosBits);                                  \
    step1[22] = half_btf(-cospi[24], step0[22], -cospi[40], step0[25], CosBits);                                 \
    step1[23] = step0[23];                                                                                       \
    step1[24] = step0[24];                                                                                       \
    step1[25] = half_btf(-cospi[40], step0[22], cospi[24], step0[25], CosBits);                                  \
    step1[26] = half_btf(cospi[24], step0[21], cospi[40], step0[26], CosBits);                                   \
    step1[27] = step0[27];                                                                                       \
    step1[28] = step0[28];                                                                                       \
    step1[29] = half_btf(-cospi[8], step0[18], cospi[56], step0[29], CosBits);                                   \
    step1[30] = half_btf(cospi[56], step0[17], cospi[8], step0[30], CosBits);                                    \
    step1[31] = step0[31];                                                                                       \
    step0[0] = half_btf(cospi[32], step1[0], cospi[32], step1[1], CosBits);                                      \
    step0[1] = half_btf(cospi[32], step1[0], -cospi[32], step1[1], CosBits);                                     \
    step0[2] = half_btf(cospi[48], step1[2], -cospi[16], step1[3], CosBits);                                     \
    step0[3] = half_btf(cospi[16], step1[2], cospi[48], step1[3], CosBits);                                      \
    step0[4] = clamp_value(step1[4] + step1[5], range);                                                          \
    step0[5] = clamp_value(step1[4] - step1[5], range);                                                          \
    step0[6] = clamp_value(-step1[6] + step1[7], range);                                                         \
    step0[7] = clamp_value(step1[6] + step1[7], range);                                                          \
    step0[8] = step1[8];                                                                                         \
    step0[9] = half_btf(-cospi[16], step1[9], cospi[48], step1[14], CosBits);                                    \
    step0[10] = half_btf(-cospi[48], step1[10], -cospi[16], step1[13], CosBits);                                 \
    step0[11] = step1[11];                                                                                       \
    step0[12] = step1[12];                                                                                       \
    step0[13] = half_btf(-cospi[16], step1[10], cospi[48], step1[13], CosBits);                                  \
    step0[14] = half_btf(cospi[48], step1[9], cospi[16], step1[14], CosBits);                                    \
    step0[15] = step1[15];                                                                                       \
    step0[16] = clamp_value(step1[16] + step1[19], range);                                                       \
    step0[17] = clamp_value(step1[17] + step1[18], range);                                                       \
    step0[18] = clamp_value(step1[17] - step1[18], range);                                                       \
    step0[19] = clamp_value(step1[16] - step1[19], range);                                                       \
    step0[20] = clamp_value(-step1[20] + step1[23], range);                                                      \
    step0[21] = clamp_value(-step1[21] + step1[22], range);                                                      \
    step0[22] = clamp_value(step1[21] + step1[22], range);                                                       \
    step0[23] = clamp_value(step1[20] + step1[23], range);                                                       \
    step0[24] = clamp_value(step1[24] + step1[27], range);                                                       \
    step0[25] = clamp_value(step1[25] + step1[26], range);                                                       \
    step0[26] = clamp_value(step1[25] - step1[26], range);                                                       \
    step0[27] = clamp_value(step1[24] - step1[27], range);                                                       \
    step0[28] = clamp_value(-step1[28] + step1[31], range);                                                      \
    step0[29] = clamp_value(-step1[29] + step1[30], range);                                                      \
    step0[30] = clamp_value(step1[29] + step1[30], range);                                                       \
    step0[31] = clamp_value(step1[28] + step1[31], range);                                                       \
    step1[0] = clamp_value(step0[0] + step0[3], range);                                                          \
    step1[1] = clamp_value(step0[1] + step0[2], range);                                                          \
    step1[2] = clamp_value(step0[1] - step0[2], range);                                                          \
    step1[3] = clamp_value(step0[0] - step0[3], range);                                                          \
    step1[4] = step0[4];                                                                                         \
    step1[5] = half_btf(-cospi[32], step0[5], cospi[32], step0[6], CosBits);                                     \
    step1[6] = half_btf(cospi[32], step0[5], cospi[32], step0[6], CosBits);                                      \
    step1[7] = step0[7];                                                                                         \
    step1[8] = clamp_value(step0[8] + step0[11], range);                                                         \
    step1[9] = clamp_value(step0[9] + step0[10], range);                                                         \
    step1[10] = clamp_value(step0[9] - step0[10], range);                                                        \
    step1[11] = clamp_value(step0[8] - step0[11], range);                                                        \
    step1[12] = clamp_value(-step0[12] + step0[15], range);                                                      \
    step1[13] = clamp_value(-step0[13] + step0[14], range);                                                      \
    step1[14] = clamp_value(step0[13] + step0[14], range);                                                       \
    step1[15] = clamp_value(step0[12] + step0[15], range);                                                       \
    step1[16] = step0[16];                                                                                       \
    step1[17] = step0[17];                                                                                       \
    step1[18] = half_btf(-cospi[16], step0[18], cospi[48], step0[29], CosBits);                                  \
    step1[19] = half_btf(-cospi[16], step0[19], cospi[48], step0[28], CosBits);                                  \
    step1[20] = half_btf(-cospi[48], step0[20], -cospi[16], step0[27], CosBits);                                 \
    step1[21] = half_btf(-cospi[48], step0[21], -cospi[16], step0[26], CosBits);                                 \
    step1[22] = step0[22];                                                                                       \
    step1[23] = step0[23];                                                                                       \
    step1[24] = step0[24];                                                                                       \
    step1[25] = step0[25];                                                                                       \
    step1[26] = half_btf(-cospi[16], step0[21], cospi[48], step0[26], CosBits);                                  \
    step1[27] = half_btf(-cospi[16], step0[20], cospi[48], step0[27], CosBits);                                  \
    step1[28] = half_btf(cospi[48], step0[19], cospi[16], step0[28], CosBits);                                   \
    step1[29] = half_btf(cospi[48], step0[18], cospi[16], step0[29], CosBits);                                   \
    step1[30] = step0[30];                                                                                       \
    step1[31] = step0[31];                                                                                       \
    step0[0] = clamp_value(step1[0] + step1[7], range);                                                          \
    step0[1] = clamp_value(step1[1] + step1[6], range);                                                          \
    step0[2] = clamp_value(step1[2] + step1[5], range);                                                          \
    step0[3] = clamp_value(step1[3] + step1[4], range);                                                          \
    step0[4] = clamp_value(step1[3] - step1[4], range);                                                          \
    step0[5] = clamp_value(step1[2] - step1[5], range);                                                          \
    step0[6] = clamp_value(step1[1] - step1[6], range);                                                          \
    step0[7] = clamp_value(step1[0] - step1[7], range);                                                          \
    step0[8] = step1[8];                                                                                         \
    step0[9] = step1[9];                                                                                         \
    step0[10] = half_btf(-cospi[32], step1[10], cospi[32], step1[13], CosBits);                                  \
    step0[11] = half_btf(-cospi[32], step1[11], cospi[32], step1[12], CosBits);                                  \
    step0[12] = half_btf(cospi[32], step1[11], cospi[32], step1[12], CosBits);                                   \
    step0[13] = half_btf(cospi[32], step1[10], cospi[32], step1[13], CosBits);                                   \
    step0[14] = step1[14];                                                                                       \
    step0[15] = step1[15];                                                                                       \
    step0[16] = clamp_value(step1[16] + step1[23], range);                                                       \
    step0[17] = clamp_value(step1[17] + step1[22], range);                                                       \
    step0[18] = clamp_value(step1[18] + step1[21], range);                                                       \
    step0[19] = clamp_value(step1[19] + step1[20], range);                                                       \
    step0[20] = clamp_value(step1[19] - step1[20], range);                                                       \
    step0[21] = clamp_value(step1[18] - step1[21], range);                                                       \
    step0[22] = clamp_value(step1[17] - step1[22], range);                                                       \
    step0[23] = clamp_value(step1[16] - step1[23], range);                                                       \
    step0[24] = clamp_value(-step1[24] + step1[31], range);                                                      \
    step0[25] = clamp_value(-step1[25] + step1[30], range);                                                      \
    step0[26] = clamp_value(-step1[26] + step1[29], range);                                                      \
    step0[27] = clamp_value(-step1[27] + step1[28], range);                                                      \
    step0[28] = clamp_value(step1[27] + step1[28], range);                                                       \
    step0[29] = clamp_value(step1[26] + step1[29], range);                                                       \
    step0[30] = clamp_value(step1[25] + step1[30], range);                                                       \
    step0[31] = clamp_value(step1[24] + step1[31], range);                                                       \
    step1[0] = clamp_value(step0[0] + step0[15], range);                                                         \
    step1[1] = clamp_value(step0[1] + step0[14], range);                                                         \
    step1[2] = clamp_value(step0[2] + step0[13], range);                                                         \
    step1[3] = clamp_value(step0[3] + step0[12], range);                                                         \
    step1[4] = clamp_value(step0[4] + step0[11], range);                                                         \
    step1[5] = clamp_value(step0[5] + step0[10], range);                                                         \
    step1[6] = clamp_value(step0[6] + step0[9], range);                                                          \
    step1[7] = clamp_value(step0[7] + step0[8], range);                                                          \
    step1[8] = clamp_value(step0[7] - step0[8], range);                                                          \
    step1[9] = clamp_value(step0[6] - step0[9], range);                                                          \
    step1[10] = clamp_value(step0[5] - step0[10], range);                                                        \
    step1[11] = clamp_value(step0[4] - step0[11], range);                                                        \
    step1[12] = clamp_value(step0[3] - step0[12], range);                                                        \
    step1[13] = clamp_value(step0[2] - step0[13], range);                                                        \
    step1[14] = clamp_value(step0[1] - step0[14], range);                                                        \
    step1[15] = clamp_value(step0[0] - step0[15], range);                                                        \
    step1[16] = step0[16];                                                                                       \
    step1[17] = step0[17];                                                                                       \
    step1[18] = step0[18];                                                                                       \
    step1[19] = step0[19];                                                                                       \
    step1[20] = half_btf(-cospi[32], step0[20], cospi[32], step0[27], CosBits);                                  \
    step1[21] = half_btf(-cospi[32], step0[21], cospi[32], step0[26], CosBits);                                  \
    step1[22] = half_btf(-cospi[32], step0[22], cospi[32], step0[25], CosBits);                                  \
    step1[23] = half_btf(-cospi[32], step0[23], cospi[32], step0[24], CosBits);                                  \
    step1[24] = half_btf(cospi[32], step0[23], cospi[32], step0[24], CosBits);                                   \
    step1[25] = half_btf(cospi[32], step0[22], cospi[32], step0[25], CosBits);                                   \
    step1[26] = half_btf(cospi[32], step0[21], cospi[32], step0[26], CosBits);                                   \
    step1[27] = half_btf(cospi[32], step0[20], cospi[32], step0[27], CosBits);                                   \
    step1[28] = step0[28];                                                                                       \
    step1[29] = step0[29];                                                                                       \
    step1[30] = step0[30];                                                                                       \
    step1[31] = step0[31];                                                                                       \
    output[out_offset + (out_stride)*0] = round_shift(clamp_value(step1[0] + step1[31], range), output_shift);   \
    output[out_offset + (out_stride)*1] = round_shift(clamp_value(step1[1] + step1[30], range), output_shift);   \
    output[out_offset + (out_stride)*2] = round_shift(clamp_value(step1[2] + step1[29], range), output_shift);   \
    output[out_offset + (out_stride)*3] = round_shift(clamp_value(step1[3] + step1[28], range), output_shift);   \
    output[out_offset + (out_stride)*4] = round_shift(clamp_value(step1[4] + step1[27], range), output_shift);   \
    output[out_offset + (out_stride)*5] = round_shift(clamp_value(step1[5] + step1[26], range), output_shift);   \
    output[out_offset + (out_stride)*6] = round_shift(clamp_value(step1[6] + step1[25], range), output_shift);   \
    output[out_offset + (out_stride)*7] = round_shift(clamp_value(step1[7] + step1[24], range), output_shift);   \
    output[out_offset + (out_stride)*8] = round_shift(clamp_value(step1[8] + step1[23], range), output_shift);   \
    output[out_offset + (out_stride)*9] = round_shift(clamp_value(step1[9] + step1[22], range), output_shift);   \
    output[out_offset + (out_stride)*10] = round_shift(clamp_value(step1[10] + step1[21], range), output_shift); \
    output[out_offset + (out_stride)*11] = round_shift(clamp_value(step1[11] + step1[20], range), output_shift); \
    output[out_offset + (out_stride)*12] = round_shift(clamp_value(step1[12] + step1[19], range), output_shift); \
    output[out_offset + (out_stride)*13] = round_shift(clamp_value(step1[13] + step1[18], range), output_shift); \
    output[out_offset + (out_stride)*14] = round_shift(clamp_value(step1[14] + step1[17], range), output_shift); \
    output[out_offset + (out_stride)*15] = round_shift(clamp_value(step1[15] + step1[16], range), output_shift); \
    output[out_offset + (out_stride)*16] = round_shift(clamp_value(step1[15] - step1[16], range), output_shift); \
    output[out_offset + (out_stride)*17] = round_shift(clamp_value(step1[14] - step1[17], range), output_shift); \
    output[out_offset + (out_stride)*18] = round_shift(clamp_value(step1[13] - step1[18], range), output_shift); \
    output[out_offset + (out_stride)*19] = round_shift(clamp_value(step1[12] - step1[19], range), output_shift); \
    output[out_offset + (out_stride)*20] = round_shift(clamp_value(step1[11] - step1[20], range), output_shift); \
    output[out_offset + (out_stride)*21] = round_shift(clamp_value(step1[10] - step1[21], range), output_shift); \
    output[out_offset + (out_stride)*22] = round_shift(clamp_value(step1[9] - step1[22], range), output_shift);  \
    output[out_offset + (out_stride)*23] = round_shift(clamp_value(step1[8] - step1[23], range), output_shift);  \
    output[out_offset + (out_stride)*24] = round_shift(clamp_value(step1[7] - step1[24], range), output_shift);  \
    output[out_offset + (out_stride)*25] = round_shift(clamp_value(step1[6] - step1[25], range), output_shift);  \
    output[out_offset + (out_stride)*26] = round_shift(clamp_value(step1[5] - step1[26], range), output_shift);  \
    output[out_offset + (out_stride)*27] = round_shift(clamp_value(step1[4] - step1[27], range), output_shift);  \
    output[out_offset + (out_stride)*28] = round_shift(clamp_value(step1[3] - step1[28], range), output_shift);  \
    output[out_offset + (out_stride)*29] = round_shift(clamp_value(step1[2] - step1[29], range), output_shift);  \
    output[out_offset + (out_stride)*30] = round_shift(clamp_value(step1[1] - step1[30], range), output_shift);  \
    output[out_offset + (out_stride)*31] = round_shift(clamp_value(step1[0] - step1[31], range), output_shift);  \
  }

/*
step0[0]  = input[in_offset + (in_stride) * 0]; \
step0[1]  = input[in_offset + (in_stride) * 32]; \
step0[2]  = input[in_offset + (in_stride) * 16]; \
step0[3]  = input[in_offset + (in_stride) * 48]; \
step0[4]  = input[in_offset + (in_stride) * 8]; \
step0[5]  = input[in_offset + (in_stride) * 40]; \
step0[6]  = input[in_offset + (in_stride) * 24]; \
step0[7]  = input[in_offset + (in_stride) * 56]; \
step0[8]  = input[in_offset + (in_stride) * 4]; \
step0[9]  = input[in_offset + (in_stride) * 36]; \
step0[10] = input[in_offset + (in_stride) * 20]; \
step0[11] = input[in_offset + (in_stride) * 52]; \
step0[12] = input[in_offset + (in_stride) * 12]; \
step0[13] = input[in_offset + (in_stride) * 44]; \
step0[14] = input[in_offset + (in_stride) * 28]; \
step0[15] = input[in_offset + (in_stride) * 60]; \
step0[16] = input[in_offset + (in_stride) * 2]; \
step0[17] = input[in_offset + (in_stride) * 34]; \
step0[18] = input[in_offset + (in_stride) * 18]; \
step0[19] = input[in_offset + (in_stride) * 50]; \
step0[20] = input[in_offset + (in_stride) * 10]; \
step0[21] = input[in_offset + (in_stride) * 42]; \
step0[22] = input[in_offset + (in_stride) * 26]; \
step0[23] = input[in_offset + (in_stride) * 58]; \
step0[24] = input[in_offset + (in_stride) * 6]; \
step0[25] = input[in_offset + (in_stride) * 38]; \
step0[26] = input[in_offset + (in_stride) * 22]; \
step0[27] = input[in_offset + (in_stride) * 54]; \
step0[28] = input[in_offset + (in_stride) * 14]; \
step0[29] = input[in_offset + (in_stride) * 46]; \
step0[30] = input[in_offset + (in_stride) * 30]; \
step0[31] = input[in_offset + (in_stride) * 62]; \
step0[32] = input[in_offset + (in_stride) * 1]; \
step0[33] = input[in_offset + (in_stride) * 33]; \
step0[34] = input[in_offset + (in_stride) * 17]; \
step0[35] = input[in_offset + (in_stride) * 49]; \
step0[36] = input[in_offset + (in_stride) * 9]; \
step0[37] = input[in_offset + (in_stride) * 41]; \
step0[38] = input[in_offset + (in_stride) * 25]; \
step0[39] = input[in_offset + (in_stride) * 57]; \
step0[40] = input[in_offset + (in_stride) * 5]; \
step0[41] = input[in_offset + (in_stride) * 37]; \
step0[42] = input[in_offset + (in_stride) * 21]; \
step0[43] = input[in_offset + (in_stride) * 53]; \
step0[44] = input[in_offset + (in_stride) * 13]; \
step0[45] = input[in_offset + (in_stride) * 45]; \
step0[46] = input[in_offset + (in_stride) * 29]; \
step0[47] = input[in_offset + (in_stride) * 61]; \
step0[48] = input[in_offset + (in_stride) * 3]; \
step0[49] = input[in_offset + (in_stride) * 35]; \
step0[50] = input[in_offset + (in_stride) * 19]; \
step0[51] = input[in_offset + (in_stride) * 51]; \
step0[52] = input[in_offset + (in_stride) * 11]; \
step0[53] = input[in_offset + (in_stride) * 43]; \
step0[54] = input[in_offset + (in_stride) * 27]; \
step0[55] = input[in_offset + (in_stride) * 59]; \
step0[56] = input[in_offset + (in_stride) * 7]; \
step0[57] = input[in_offset + (in_stride) * 39]; \
step0[58] = input[in_offset + (in_stride) * 23]; \
step0[59] = input[in_offset + (in_stride) * 55]; \
step0[60] = input[in_offset + (in_stride) * 15]; \
step0[61] = input[in_offset + (in_stride) * 47]; \
step0[62] = input[in_offset + (in_stride) * 31]; \
step0[63] = input[in_offset + (in_stride) * 63]; \
*/

#define IDCT64(input, in_offset, in_stride, output, out_offset, out_stride, range, output_shift)                 \
  {                                                                                                              \
    int step0[64];                                                                                               \
    int step1[64];                                                                                               \
    step0[0] = clamp_value(input[in_offset + (in_stride)*0], range);                                             \
    step0[1] = 0;                                                                                                \
    step0[2] = clamp_value(input[in_offset + (in_stride)*16], range);                                            \
    step0[3] = 0;                                                                                                \
    step0[4] = clamp_value(input[in_offset + (in_stride)*8], range);                                             \
    step0[5] = 0;                                                                                                \
    step0[6] = clamp_value(input[in_offset + (in_stride)*24], range);                                            \
    step0[7] = 0;                                                                                                \
    step0[8] = clamp_value(input[in_offset + (in_stride)*4], range);                                             \
    step0[9] = 0;                                                                                                \
    step0[10] = clamp_value(input[in_offset + (in_stride)*20], range);                                           \
    step0[11] = 0;                                                                                               \
    step0[12] = clamp_value(input[in_offset + (in_stride)*12], range);                                           \
    step0[13] = 0;                                                                                               \
    step0[14] = clamp_value(input[in_offset + (in_stride)*28], range);                                           \
    step0[15] = 0;                                                                                               \
    step0[16] = clamp_value(input[in_offset + (in_stride)*2], range);                                            \
    step0[17] = 0;                                                                                               \
    step0[18] = clamp_value(input[in_offset + (in_stride)*18], range);                                           \
    step0[19] = 0;                                                                                               \
    step0[20] = clamp_value(input[in_offset + (in_stride)*10], range);                                           \
    step0[21] = 0;                                                                                               \
    step0[22] = clamp_value(input[in_offset + (in_stride)*26], range);                                           \
    step0[23] = 0;                                                                                               \
    step0[24] = clamp_value(input[in_offset + (in_stride)*6], range);                                            \
    step0[25] = 0;                                                                                               \
    step0[26] = clamp_value(input[in_offset + (in_stride)*22], range);                                           \
    step0[27] = 0;                                                                                               \
    step0[28] = clamp_value(input[in_offset + (in_stride)*14], range);                                           \
    step0[29] = 0;                                                                                               \
    step0[30] = clamp_value(input[in_offset + (in_stride)*30], range);                                           \
    step0[31] = 0;                                                                                               \
    step0[32] = clamp_value(input[in_offset + (in_stride)*1], range);                                            \
    step0[33] = 0;                                                                                               \
    step0[34] = clamp_value(input[in_offset + (in_stride)*17], range);                                           \
    step0[35] = 0;                                                                                               \
    step0[36] = clamp_value(input[in_offset + (in_stride)*9], range);                                            \
    step0[37] = 0;                                                                                               \
    step0[38] = clamp_value(input[in_offset + (in_stride)*25], range);                                           \
    step0[39] = 0;                                                                                               \
    step0[40] = clamp_value(input[in_offset + (in_stride)*5], range);                                            \
    step0[41] = 0;                                                                                               \
    step0[42] = clamp_value(input[in_offset + (in_stride)*21], range);                                           \
    step0[43] = 0;                                                                                               \
    step0[44] = clamp_value(input[in_offset + (in_stride)*13], range);                                           \
    step0[45] = 0;                                                                                               \
    step0[46] = clamp_value(input[in_offset + (in_stride)*29], range);                                           \
    step0[47] = 0;                                                                                               \
    step0[48] = clamp_value(input[in_offset + (in_stride)*3], range);                                            \
    step0[49] = 0;                                                                                               \
    step0[50] = clamp_value(input[in_offset + (in_stride)*19], range);                                           \
    step0[51] = 0;                                                                                               \
    step0[52] = clamp_value(input[in_offset + (in_stride)*11], range);                                           \
    step0[53] = 0;                                                                                               \
    step0[54] = clamp_value(input[in_offset + (in_stride)*27], range);                                           \
    step0[55] = 0;                                                                                               \
    step0[56] = clamp_value(input[in_offset + (in_stride)*7], range);                                            \
    step0[57] = 0;                                                                                               \
    step0[58] = clamp_value(input[in_offset + (in_stride)*23], range);                                           \
    step0[59] = 0;                                                                                               \
    step0[60] = clamp_value(input[in_offset + (in_stride)*15], range);                                           \
    step0[61] = 0;                                                                                               \
    step0[62] = clamp_value(input[in_offset + (in_stride)*31], range);                                           \
    step0[63] = 0;                                                                                               \
    step1[0] = step0[0];                                                                                         \
    step1[1] = step0[1];                                                                                         \
    step1[2] = step0[2];                                                                                         \
    step1[3] = step0[3];                                                                                         \
    step1[4] = step0[4];                                                                                         \
    step1[5] = step0[5];                                                                                         \
    step1[6] = step0[6];                                                                                         \
    step1[7] = step0[7];                                                                                         \
    step1[8] = step0[8];                                                                                         \
    step1[9] = step0[9];                                                                                         \
    step1[10] = step0[10];                                                                                       \
    step1[11] = step0[11];                                                                                       \
    step1[12] = step0[12];                                                                                       \
    step1[13] = step0[13];                                                                                       \
    step1[14] = step0[14];                                                                                       \
    step1[15] = step0[15];                                                                                       \
    step1[16] = step0[16];                                                                                       \
    step1[17] = step0[17];                                                                                       \
    step1[18] = step0[18];                                                                                       \
    step1[19] = step0[19];                                                                                       \
    step1[20] = step0[20];                                                                                       \
    step1[21] = step0[21];                                                                                       \
    step1[22] = step0[22];                                                                                       \
    step1[23] = step0[23];                                                                                       \
    step1[24] = step0[24];                                                                                       \
    step1[25] = step0[25];                                                                                       \
    step1[26] = step0[26];                                                                                       \
    step1[27] = step0[27];                                                                                       \
    step1[28] = step0[28];                                                                                       \
    step1[29] = step0[29];                                                                                       \
    step1[30] = step0[30];                                                                                       \
    step1[31] = step0[31];                                                                                       \
    step1[32] = half_btf(cospi[63], step0[32], -cospi[1], step0[63], CosBits);                                   \
    step1[33] = half_btf(cospi[31], step0[33], -cospi[33], step0[62], CosBits);                                  \
    step1[34] = half_btf(cospi[47], step0[34], -cospi[17], step0[61], CosBits);                                  \
    step1[35] = half_btf(cospi[15], step0[35], -cospi[49], step0[60], CosBits);                                  \
    step1[36] = half_btf(cospi[55], step0[36], -cospi[9], step0[59], CosBits);                                   \
    step1[37] = half_btf(cospi[23], step0[37], -cospi[41], step0[58], CosBits);                                  \
    step1[38] = half_btf(cospi[39], step0[38], -cospi[25], step0[57], CosBits);                                  \
    step1[39] = half_btf(cospi[7], step0[39], -cospi[57], step0[56], CosBits);                                   \
    step1[40] = half_btf(cospi[59], step0[40], -cospi[5], step0[55], CosBits);                                   \
    step1[41] = half_btf(cospi[27], step0[41], -cospi[37], step0[54], CosBits);                                  \
    step1[42] = half_btf(cospi[43], step0[42], -cospi[21], step0[53], CosBits);                                  \
    step1[43] = half_btf(cospi[11], step0[43], -cospi[53], step0[52], CosBits);                                  \
    step1[44] = half_btf(cospi[51], step0[44], -cospi[13], step0[51], CosBits);                                  \
    step1[45] = half_btf(cospi[19], step0[45], -cospi[45], step0[50], CosBits);                                  \
    step1[46] = half_btf(cospi[35], step0[46], -cospi[29], step0[49], CosBits);                                  \
    step1[47] = half_btf(cospi[3], step0[47], -cospi[61], step0[48], CosBits);                                   \
    step1[48] = half_btf(cospi[61], step0[47], cospi[3], step0[48], CosBits);                                    \
    step1[49] = half_btf(cospi[29], step0[46], cospi[35], step0[49], CosBits);                                   \
    step1[50] = half_btf(cospi[45], step0[45], cospi[19], step0[50], CosBits);                                   \
    step1[51] = half_btf(cospi[13], step0[44], cospi[51], step0[51], CosBits);                                   \
    step1[52] = half_btf(cospi[53], step0[43], cospi[11], step0[52], CosBits);                                   \
    step1[53] = half_btf(cospi[21], step0[42], cospi[43], step0[53], CosBits);                                   \
    step1[54] = half_btf(cospi[37], step0[41], cospi[27], step0[54], CosBits);                                   \
    step1[55] = half_btf(cospi[5], step0[40], cospi[59], step0[55], CosBits);                                    \
    step1[56] = half_btf(cospi[57], step0[39], cospi[7], step0[56], CosBits);                                    \
    step1[57] = half_btf(cospi[25], step0[38], cospi[39], step0[57], CosBits);                                   \
    step1[58] = half_btf(cospi[41], step0[37], cospi[23], step0[58], CosBits);                                   \
    step1[59] = half_btf(cospi[9], step0[36], cospi[55], step0[59], CosBits);                                    \
    step1[60] = half_btf(cospi[49], step0[35], cospi[15], step0[60], CosBits);                                   \
    step1[61] = half_btf(cospi[17], step0[34], cospi[47], step0[61], CosBits);                                   \
    step1[62] = half_btf(cospi[33], step0[33], cospi[31], step0[62], CosBits);                                   \
    step1[63] = half_btf(cospi[1], step0[32], cospi[63], step0[63], CosBits);                                    \
    step0[0] = step1[0];                                                                                         \
    step0[1] = step1[1];                                                                                         \
    step0[2] = step1[2];                                                                                         \
    step0[3] = step1[3];                                                                                         \
    step0[4] = step1[4];                                                                                         \
    step0[5] = step1[5];                                                                                         \
    step0[6] = step1[6];                                                                                         \
    step0[7] = step1[7];                                                                                         \
    step0[8] = step1[8];                                                                                         \
    step0[9] = step1[9];                                                                                         \
    step0[10] = step1[10];                                                                                       \
    step0[11] = step1[11];                                                                                       \
    step0[12] = step1[12];                                                                                       \
    step0[13] = step1[13];                                                                                       \
    step0[14] = step1[14];                                                                                       \
    step0[15] = step1[15];                                                                                       \
    step0[16] = half_btf(cospi[62], step1[16], -cospi[2], step1[31], CosBits);                                   \
    step0[17] = half_btf(cospi[30], step1[17], -cospi[34], step1[30], CosBits);                                  \
    step0[18] = half_btf(cospi[46], step1[18], -cospi[18], step1[29], CosBits);                                  \
    step0[19] = half_btf(cospi[14], step1[19], -cospi[50], step1[28], CosBits);                                  \
    step0[20] = half_btf(cospi[54], step1[20], -cospi[10], step1[27], CosBits);                                  \
    step0[21] = half_btf(cospi[22], step1[21], -cospi[42], step1[26], CosBits);                                  \
    step0[22] = half_btf(cospi[38], step1[22], -cospi[26], step1[25], CosBits);                                  \
    step0[23] = half_btf(cospi[6], step1[23], -cospi[58], step1[24], CosBits);                                   \
    step0[24] = half_btf(cospi[58], step1[23], cospi[6], step1[24], CosBits);                                    \
    step0[25] = half_btf(cospi[26], step1[22], cospi[38], step1[25], CosBits);                                   \
    step0[26] = half_btf(cospi[42], step1[21], cospi[22], step1[26], CosBits);                                   \
    step0[27] = half_btf(cospi[10], step1[20], cospi[54], step1[27], CosBits);                                   \
    step0[28] = half_btf(cospi[50], step1[19], cospi[14], step1[28], CosBits);                                   \
    step0[29] = half_btf(cospi[18], step1[18], cospi[46], step1[29], CosBits);                                   \
    step0[30] = half_btf(cospi[34], step1[17], cospi[30], step1[30], CosBits);                                   \
    step0[31] = half_btf(cospi[2], step1[16], cospi[62], step1[31], CosBits);                                    \
    step0[32] = clamp_value(step1[32] + step1[33], range);                                                       \
    step0[33] = clamp_value(step1[32] - step1[33], range);                                                       \
    step0[34] = clamp_value(-step1[34] + step1[35], range);                                                      \
    step0[35] = clamp_value(step1[34] + step1[35], range);                                                       \
    step0[36] = clamp_value(step1[36] + step1[37], range);                                                       \
    step0[37] = clamp_value(step1[36] - step1[37], range);                                                       \
    step0[38] = clamp_value(-step1[38] + step1[39], range);                                                      \
    step0[39] = clamp_value(step1[38] + step1[39], range);                                                       \
    step0[40] = clamp_value(step1[40] + step1[41], range);                                                       \
    step0[41] = clamp_value(step1[40] - step1[41], range);                                                       \
    step0[42] = clamp_value(-step1[42] + step1[43], range);                                                      \
    step0[43] = clamp_value(step1[42] + step1[43], range);                                                       \
    step0[44] = clamp_value(step1[44] + step1[45], range);                                                       \
    step0[45] = clamp_value(step1[44] - step1[45], range);                                                       \
    step0[46] = clamp_value(-step1[46] + step1[47], range);                                                      \
    step0[47] = clamp_value(step1[46] + step1[47], range);                                                       \
    step0[48] = clamp_value(step1[48] + step1[49], range);                                                       \
    step0[49] = clamp_value(step1[48] - step1[49], range);                                                       \
    step0[50] = clamp_value(-step1[50] + step1[51], range);                                                      \
    step0[51] = clamp_value(step1[50] + step1[51], range);                                                       \
    step0[52] = clamp_value(step1[52] + step1[53], range);                                                       \
    step0[53] = clamp_value(step1[52] - step1[53], range);                                                       \
    step0[54] = clamp_value(-step1[54] + step1[55], range);                                                      \
    step0[55] = clamp_value(step1[54] + step1[55], range);                                                       \
    step0[56] = clamp_value(step1[56] + step1[57], range);                                                       \
    step0[57] = clamp_value(step1[56] - step1[57], range);                                                       \
    step0[58] = clamp_value(-step1[58] + step1[59], range);                                                      \
    step0[59] = clamp_value(step1[58] + step1[59], range);                                                       \
    step0[60] = clamp_value(step1[60] + step1[61], range);                                                       \
    step0[61] = clamp_value(step1[60] - step1[61], range);                                                       \
    step0[62] = clamp_value(-step1[62] + step1[63], range);                                                      \
    step0[63] = clamp_value(step1[62] + step1[63], range);                                                       \
    step1[0] = step0[0];                                                                                         \
    step1[1] = step0[1];                                                                                         \
    step1[2] = step0[2];                                                                                         \
    step1[3] = step0[3];                                                                                         \
    step1[4] = step0[4];                                                                                         \
    step1[5] = step0[5];                                                                                         \
    step1[6] = step0[6];                                                                                         \
    step1[7] = step0[7];                                                                                         \
    step1[8] = half_btf(cospi[60], step0[8], -cospi[4], step0[15], CosBits);                                     \
    step1[9] = half_btf(cospi[28], step0[9], -cospi[36], step0[14], CosBits);                                    \
    step1[10] = half_btf(cospi[44], step0[10], -cospi[20], step0[13], CosBits);                                  \
    step1[11] = half_btf(cospi[12], step0[11], -cospi[52], step0[12], CosBits);                                  \
    step1[12] = half_btf(cospi[52], step0[11], cospi[12], step0[12], CosBits);                                   \
    step1[13] = half_btf(cospi[20], step0[10], cospi[44], step0[13], CosBits);                                   \
    step1[14] = half_btf(cospi[36], step0[9], cospi[28], step0[14], CosBits);                                    \
    step1[15] = half_btf(cospi[4], step0[8], cospi[60], step0[15], CosBits);                                     \
    step1[16] = clamp_value(step0[16] + step0[17], range);                                                       \
    step1[17] = clamp_value(step0[16] - step0[17], range);                                                       \
    step1[18] = clamp_value(-step0[18] + step0[19], range);                                                      \
    step1[19] = clamp_value(step0[18] + step0[19], range);                                                       \
    step1[20] = clamp_value(step0[20] + step0[21], range);                                                       \
    step1[21] = clamp_value(step0[20] - step0[21], range);                                                       \
    step1[22] = clamp_value(-step0[22] + step0[23], range);                                                      \
    step1[23] = clamp_value(step0[22] + step0[23], range);                                                       \
    step1[24] = clamp_value(step0[24] + step0[25], range);                                                       \
    step1[25] = clamp_value(step0[24] - step0[25], range);                                                       \
    step1[26] = clamp_value(-step0[26] + step0[27], range);                                                      \
    step1[27] = clamp_value(step0[26] + step0[27], range);                                                       \
    step1[28] = clamp_value(step0[28] + step0[29], range);                                                       \
    step1[29] = clamp_value(step0[28] - step0[29], range);                                                       \
    step1[30] = clamp_value(-step0[30] + step0[31], range);                                                      \
    step1[31] = clamp_value(step0[30] + step0[31], range);                                                       \
    step1[32] = step0[32];                                                                                       \
    step1[33] = half_btf(-cospi[4], step0[33], cospi[60], step0[62], CosBits);                                   \
    step1[34] = half_btf(-cospi[60], step0[34], -cospi[4], step0[61], CosBits);                                  \
    step1[35] = step0[35];                                                                                       \
    step1[36] = step0[36];                                                                                       \
    step1[37] = half_btf(-cospi[36], step0[37], cospi[28], step0[58], CosBits);                                  \
    step1[38] = half_btf(-cospi[28], step0[38], -cospi[36], step0[57], CosBits);                                 \
    step1[39] = step0[39];                                                                                       \
    step1[40] = step0[40];                                                                                       \
    step1[41] = half_btf(-cospi[20], step0[41], cospi[44], step0[54], CosBits);                                  \
    step1[42] = half_btf(-cospi[44], step0[42], -cospi[20], step0[53], CosBits);                                 \
    step1[43] = step0[43];                                                                                       \
    step1[44] = step0[44];                                                                                       \
    step1[45] = half_btf(-cospi[52], step0[45], cospi[12], step0[50], CosBits);                                  \
    step1[46] = half_btf(-cospi[12], step0[46], -cospi[52], step0[49], CosBits);                                 \
    step1[47] = step0[47];                                                                                       \
    step1[48] = step0[48];                                                                                       \
    step1[49] = half_btf(-cospi[52], step0[46], cospi[12], step0[49], CosBits);                                  \
    step1[50] = half_btf(cospi[12], step0[45], cospi[52], step0[50], CosBits);                                   \
    step1[51] = step0[51];                                                                                       \
    step1[52] = step0[52];                                                                                       \
    step1[53] = half_btf(-cospi[20], step0[42], cospi[44], step0[53], CosBits);                                  \
    step1[54] = half_btf(cospi[44], step0[41], cospi[20], step0[54], CosBits);                                   \
    step1[55] = step0[55];                                                                                       \
    step1[56] = step0[56];                                                                                       \
    step1[57] = half_btf(-cospi[36], step0[38], cospi[28], step0[57], CosBits);                                  \
    step1[58] = half_btf(cospi[28], step0[37], cospi[36], step0[58], CosBits);                                   \
    step1[59] = step0[59];                                                                                       \
    step1[60] = step0[60];                                                                                       \
    step1[61] = half_btf(-cospi[4], step0[34], cospi[60], step0[61], CosBits);                                   \
    step1[62] = half_btf(cospi[60], step0[33], cospi[4], step0[62], CosBits);                                    \
    step1[63] = step0[63];                                                                                       \
    step0[0] = step1[0];                                                                                         \
    step0[1] = step1[1];                                                                                         \
    step0[2] = step1[2];                                                                                         \
    step0[3] = step1[3];                                                                                         \
    step0[4] = half_btf(cospi[56], step1[4], -cospi[8], step1[7], CosBits);                                      \
    step0[5] = half_btf(cospi[24], step1[5], -cospi[40], step1[6], CosBits);                                     \
    step0[6] = half_btf(cospi[40], step1[5], cospi[24], step1[6], CosBits);                                      \
    step0[7] = half_btf(cospi[8], step1[4], cospi[56], step1[7], CosBits);                                       \
    step0[8] = clamp_value(step1[8] + step1[9], range);                                                          \
    step0[9] = clamp_value(step1[8] - step1[9], range);                                                          \
    step0[10] = clamp_value(-step1[10] + step1[11], range);                                                      \
    step0[11] = clamp_value(step1[10] + step1[11], range);                                                       \
    step0[12] = clamp_value(step1[12] + step1[13], range);                                                       \
    step0[13] = clamp_value(step1[12] - step1[13], range);                                                       \
    step0[14] = clamp_value(-step1[14] + step1[15], range);                                                      \
    step0[15] = clamp_value(step1[14] + step1[15], range);                                                       \
    step0[16] = step1[16];                                                                                       \
    step0[17] = half_btf(-cospi[8], step1[17], cospi[56], step1[30], CosBits);                                   \
    step0[18] = half_btf(-cospi[56], step1[18], -cospi[8], step1[29], CosBits);                                  \
    step0[19] = step1[19];                                                                                       \
    step0[20] = step1[20];                                                                                       \
    step0[21] = half_btf(-cospi[40], step1[21], cospi[24], step1[26], CosBits);                                  \
    step0[22] = half_btf(-cospi[24], step1[22], -cospi[40], step1[25], CosBits);                                 \
    step0[23] = step1[23];                                                                                       \
    step0[24] = step1[24];                                                                                       \
    step0[25] = half_btf(-cospi[40], step1[22], cospi[24], step1[25], CosBits);                                  \
    step0[26] = half_btf(cospi[24], step1[21], cospi[40], step1[26], CosBits);                                   \
    step0[27] = step1[27];                                                                                       \
    step0[28] = step1[28];                                                                                       \
    step0[29] = half_btf(-cospi[8], step1[18], cospi[56], step1[29], CosBits);                                   \
    step0[30] = half_btf(cospi[56], step1[17], cospi[8], step1[30], CosBits);                                    \
    step0[31] = step1[31];                                                                                       \
    step0[32] = clamp_value(step1[32] + step1[35], range);                                                       \
    step0[33] = clamp_value(step1[33] + step1[34], range);                                                       \
    step0[34] = clamp_value(step1[33] - step1[34], range);                                                       \
    step0[35] = clamp_value(step1[32] - step1[35], range);                                                       \
    step0[36] = clamp_value(-step1[36] + step1[39], range);                                                      \
    step0[37] = clamp_value(-step1[37] + step1[38], range);                                                      \
    step0[38] = clamp_value(step1[37] + step1[38], range);                                                       \
    step0[39] = clamp_value(step1[36] + step1[39], range);                                                       \
    step0[40] = clamp_value(step1[40] + step1[43], range);                                                       \
    step0[41] = clamp_value(step1[41] + step1[42], range);                                                       \
    step0[42] = clamp_value(step1[41] - step1[42], range);                                                       \
    step0[43] = clamp_value(step1[40] - step1[43], range);                                                       \
    step0[44] = clamp_value(-step1[44] + step1[47], range);                                                      \
    step0[45] = clamp_value(-step1[45] + step1[46], range);                                                      \
    step0[46] = clamp_value(step1[45] + step1[46], range);                                                       \
    step0[47] = clamp_value(step1[44] + step1[47], range);                                                       \
    step0[48] = clamp_value(step1[48] + step1[51], range);                                                       \
    step0[49] = clamp_value(step1[49] + step1[50], range);                                                       \
    step0[50] = clamp_value(step1[49] - step1[50], range);                                                       \
    step0[51] = clamp_value(step1[48] - step1[51], range);                                                       \
    step0[52] = clamp_value(-step1[52] + step1[55], range);                                                      \
    step0[53] = clamp_value(-step1[53] + step1[54], range);                                                      \
    step0[54] = clamp_value(step1[53] + step1[54], range);                                                       \
    step0[55] = clamp_value(step1[52] + step1[55], range);                                                       \
    step0[56] = clamp_value(step1[56] + step1[59], range);                                                       \
    step0[57] = clamp_value(step1[57] + step1[58], range);                                                       \
    step0[58] = clamp_value(step1[57] - step1[58], range);                                                       \
    step0[59] = clamp_value(step1[56] - step1[59], range);                                                       \
    step0[60] = clamp_value(-step1[60] + step1[63], range);                                                      \
    step0[61] = clamp_value(-step1[61] + step1[62], range);                                                      \
    step0[62] = clamp_value(step1[61] + step1[62], range);                                                       \
    step0[63] = clamp_value(step1[60] + step1[63], range);                                                       \
    step1[0] = half_btf(cospi[32], step0[0], cospi[32], step0[1], CosBits);                                      \
    step1[1] = half_btf(cospi[32], step0[0], -cospi[32], step0[1], CosBits);                                     \
    step1[2] = half_btf(cospi[48], step0[2], -cospi[16], step0[3], CosBits);                                     \
    step1[3] = half_btf(cospi[16], step0[2], cospi[48], step0[3], CosBits);                                      \
    step1[4] = clamp_value(step0[4] + step0[5], range);                                                          \
    step1[5] = clamp_value(step0[4] - step0[5], range);                                                          \
    step1[6] = clamp_value(-step0[6] + step0[7], range);                                                         \
    step1[7] = clamp_value(step0[6] + step0[7], range);                                                          \
    step1[8] = step0[8];                                                                                         \
    step1[9] = half_btf(-cospi[16], step0[9], cospi[48], step0[14], CosBits);                                    \
    step1[10] = half_btf(-cospi[48], step0[10], -cospi[16], step0[13], CosBits);                                 \
    step1[11] = step0[11];                                                                                       \
    step1[12] = step0[12];                                                                                       \
    step1[13] = half_btf(-cospi[16], step0[10], cospi[48], step0[13], CosBits);                                  \
    step1[14] = half_btf(cospi[48], step0[9], cospi[16], step0[14], CosBits);                                    \
    step1[15] = step0[15];                                                                                       \
    step1[16] = clamp_value(step0[16] + step0[19], range);                                                       \
    step1[17] = clamp_value(step0[17] + step0[18], range);                                                       \
    step1[18] = clamp_value(step0[17] - step0[18], range);                                                       \
    step1[19] = clamp_value(step0[16] - step0[19], range);                                                       \
    step1[20] = clamp_value(-step0[20] + step0[23], range);                                                      \
    step1[21] = clamp_value(-step0[21] + step0[22], range);                                                      \
    step1[22] = clamp_value(step0[21] + step0[22], range);                                                       \
    step1[23] = clamp_value(step0[20] + step0[23], range);                                                       \
    step1[24] = clamp_value(step0[24] + step0[27], range);                                                       \
    step1[25] = clamp_value(step0[25] + step0[26], range);                                                       \
    step1[26] = clamp_value(step0[25] - step0[26], range);                                                       \
    step1[27] = clamp_value(step0[24] - step0[27], range);                                                       \
    step1[28] = clamp_value(-step0[28] + step0[31], range);                                                      \
    step1[29] = clamp_value(-step0[29] + step0[30], range);                                                      \
    step1[30] = clamp_value(step0[29] + step0[30], range);                                                       \
    step1[31] = clamp_value(step0[28] + step0[31], range);                                                       \
    step1[32] = step0[32];                                                                                       \
    step1[33] = step0[33];                                                                                       \
    step1[34] = half_btf(-cospi[8], step0[34], cospi[56], step0[61], CosBits);                                   \
    step1[35] = half_btf(-cospi[8], step0[35], cospi[56], step0[60], CosBits);                                   \
    step1[36] = half_btf(-cospi[56], step0[36], -cospi[8], step0[59], CosBits);                                  \
    step1[37] = half_btf(-cospi[56], step0[37], -cospi[8], step0[58], CosBits);                                  \
    step1[38] = step0[38];                                                                                       \
    step1[39] = step0[39];                                                                                       \
    step1[40] = step0[40];                                                                                       \
    step1[41] = step0[41];                                                                                       \
    step1[42] = half_btf(-cospi[40], step0[42], cospi[24], step0[53], CosBits);                                  \
    step1[43] = half_btf(-cospi[40], step0[43], cospi[24], step0[52], CosBits);                                  \
    step1[44] = half_btf(-cospi[24], step0[44], -cospi[40], step0[51], CosBits);                                 \
    step1[45] = half_btf(-cospi[24], step0[45], -cospi[40], step0[50], CosBits);                                 \
    step1[46] = step0[46];                                                                                       \
    step1[47] = step0[47];                                                                                       \
    step1[48] = step0[48];                                                                                       \
    step1[49] = step0[49];                                                                                       \
    step1[50] = half_btf(-cospi[40], step0[45], cospi[24], step0[50], CosBits);                                  \
    step1[51] = half_btf(-cospi[40], step0[44], cospi[24], step0[51], CosBits);                                  \
    step1[52] = half_btf(cospi[24], step0[43], cospi[40], step0[52], CosBits);                                   \
    step1[53] = half_btf(cospi[24], step0[42], cospi[40], step0[53], CosBits);                                   \
    step1[54] = step0[54];                                                                                       \
    step1[55] = step0[55];                                                                                       \
    step1[56] = step0[56];                                                                                       \
    step1[57] = step0[57];                                                                                       \
    step1[58] = half_btf(-cospi[8], step0[37], cospi[56], step0[58], CosBits);                                   \
    step1[59] = half_btf(-cospi[8], step0[36], cospi[56], step0[59], CosBits);                                   \
    step1[60] = half_btf(cospi[56], step0[35], cospi[8], step0[60], CosBits);                                    \
    step1[61] = half_btf(cospi[56], step0[34], cospi[8], step0[61], CosBits);                                    \
    step1[62] = step0[62];                                                                                       \
    step1[63] = step0[63];                                                                                       \
    step0[0] = clamp_value(step1[0] + step1[3], range);                                                          \
    step0[1] = clamp_value(step1[1] + step1[2], range);                                                          \
    step0[2] = clamp_value(step1[1] - step1[2], range);                                                          \
    step0[3] = clamp_value(step1[0] - step1[3], range);                                                          \
    step0[4] = step1[4];                                                                                         \
    step0[5] = half_btf(-cospi[32], step1[5], cospi[32], step1[6], CosBits);                                     \
    step0[6] = half_btf(cospi[32], step1[5], cospi[32], step1[6], CosBits);                                      \
    step0[7] = step1[7];                                                                                         \
    step0[8] = clamp_value(step1[8] + step1[11], range);                                                         \
    step0[9] = clamp_value(step1[9] + step1[10], range);                                                         \
    step0[10] = clamp_value(step1[9] - step1[10], range);                                                        \
    step0[11] = clamp_value(step1[8] - step1[11], range);                                                        \
    step0[12] = clamp_value(-step1[12] + step1[15], range);                                                      \
    step0[13] = clamp_value(-step1[13] + step1[14], range);                                                      \
    step0[14] = clamp_value(step1[13] + step1[14], range);                                                       \
    step0[15] = clamp_value(step1[12] + step1[15], range);                                                       \
    step0[16] = step1[16];                                                                                       \
    step0[17] = step1[17];                                                                                       \
    step0[18] = half_btf(-cospi[16], step1[18], cospi[48], step1[29], CosBits);                                  \
    step0[19] = half_btf(-cospi[16], step1[19], cospi[48], step1[28], CosBits);                                  \
    step0[20] = half_btf(-cospi[48], step1[20], -cospi[16], step1[27], CosBits);                                 \
    step0[21] = half_btf(-cospi[48], step1[21], -cospi[16], step1[26], CosBits);                                 \
    step0[22] = step1[22];                                                                                       \
    step0[23] = step1[23];                                                                                       \
    step0[24] = step1[24];                                                                                       \
    step0[25] = step1[25];                                                                                       \
    step0[26] = half_btf(-cospi[16], step1[21], cospi[48], step1[26], CosBits);                                  \
    step0[27] = half_btf(-cospi[16], step1[20], cospi[48], step1[27], CosBits);                                  \
    step0[28] = half_btf(cospi[48], step1[19], cospi[16], step1[28], CosBits);                                   \
    step0[29] = half_btf(cospi[48], step1[18], cospi[16], step1[29], CosBits);                                   \
    step0[30] = step1[30];                                                                                       \
    step0[31] = step1[31];                                                                                       \
    step0[32] = clamp_value(step1[32] + step1[39], range);                                                       \
    step0[33] = clamp_value(step1[33] + step1[38], range);                                                       \
    step0[34] = clamp_value(step1[34] + step1[37], range);                                                       \
    step0[35] = clamp_value(step1[35] + step1[36], range);                                                       \
    step0[36] = clamp_value(step1[35] - step1[36], range);                                                       \
    step0[37] = clamp_value(step1[34] - step1[37], range);                                                       \
    step0[38] = clamp_value(step1[33] - step1[38], range);                                                       \
    step0[39] = clamp_value(step1[32] - step1[39], range);                                                       \
    step0[40] = clamp_value(-step1[40] + step1[47], range);                                                      \
    step0[41] = clamp_value(-step1[41] + step1[46], range);                                                      \
    step0[42] = clamp_value(-step1[42] + step1[45], range);                                                      \
    step0[43] = clamp_value(-step1[43] + step1[44], range);                                                      \
    step0[44] = clamp_value(step1[43] + step1[44], range);                                                       \
    step0[45] = clamp_value(step1[42] + step1[45], range);                                                       \
    step0[46] = clamp_value(step1[41] + step1[46], range);                                                       \
    step0[47] = clamp_value(step1[40] + step1[47], range);                                                       \
    step0[48] = clamp_value(step1[48] + step1[55], range);                                                       \
    step0[49] = clamp_value(step1[49] + step1[54], range);                                                       \
    step0[50] = clamp_value(step1[50] + step1[53], range);                                                       \
    step0[51] = clamp_value(step1[51] + step1[52], range);                                                       \
    step0[52] = clamp_value(step1[51] - step1[52], range);                                                       \
    step0[53] = clamp_value(step1[50] - step1[53], range);                                                       \
    step0[54] = clamp_value(step1[49] - step1[54], range);                                                       \
    step0[55] = clamp_value(step1[48] - step1[55], range);                                                       \
    step0[56] = clamp_value(-step1[56] + step1[63], range);                                                      \
    step0[57] = clamp_value(-step1[57] + step1[62], range);                                                      \
    step0[58] = clamp_value(-step1[58] + step1[61], range);                                                      \
    step0[59] = clamp_value(-step1[59] + step1[60], range);                                                      \
    step0[60] = clamp_value(step1[59] + step1[60], range);                                                       \
    step0[61] = clamp_value(step1[58] + step1[61], range);                                                       \
    step0[62] = clamp_value(step1[57] + step1[62], range);                                                       \
    step0[63] = clamp_value(step1[56] + step1[63], range);                                                       \
    step1[0] = clamp_value(step0[0] + step0[7], range);                                                          \
    step1[1] = clamp_value(step0[1] + step0[6], range);                                                          \
    step1[2] = clamp_value(step0[2] + step0[5], range);                                                          \
    step1[3] = clamp_value(step0[3] + step0[4], range);                                                          \
    step1[4] = clamp_value(step0[3] - step0[4], range);                                                          \
    step1[5] = clamp_value(step0[2] - step0[5], range);                                                          \
    step1[6] = clamp_value(step0[1] - step0[6], range);                                                          \
    step1[7] = clamp_value(step0[0] - step0[7], range);                                                          \
    step1[8] = step0[8];                                                                                         \
    step1[9] = step0[9];                                                                                         \
    step1[10] = half_btf(-cospi[32], step0[10], cospi[32], step0[13], CosBits);                                  \
    step1[11] = half_btf(-cospi[32], step0[11], cospi[32], step0[12], CosBits);                                  \
    step1[12] = half_btf(cospi[32], step0[11], cospi[32], step0[12], CosBits);                                   \
    step1[13] = half_btf(cospi[32], step0[10], cospi[32], step0[13], CosBits);                                   \
    step1[14] = step0[14];                                                                                       \
    step1[15] = step0[15];                                                                                       \
    step1[16] = clamp_value(step0[16] + step0[23], range);                                                       \
    step1[17] = clamp_value(step0[17] + step0[22], range);                                                       \
    step1[18] = clamp_value(step0[18] + step0[21], range);                                                       \
    step1[19] = clamp_value(step0[19] + step0[20], range);                                                       \
    step1[20] = clamp_value(step0[19] - step0[20], range);                                                       \
    step1[21] = clamp_value(step0[18] - step0[21], range);                                                       \
    step1[22] = clamp_value(step0[17] - step0[22], range);                                                       \
    step1[23] = clamp_value(step0[16] - step0[23], range);                                                       \
    step1[24] = clamp_value(-step0[24] + step0[31], range);                                                      \
    step1[25] = clamp_value(-step0[25] + step0[30], range);                                                      \
    step1[26] = clamp_value(-step0[26] + step0[29], range);                                                      \
    step1[27] = clamp_value(-step0[27] + step0[28], range);                                                      \
    step1[28] = clamp_value(step0[27] + step0[28], range);                                                       \
    step1[29] = clamp_value(step0[26] + step0[29], range);                                                       \
    step1[30] = clamp_value(step0[25] + step0[30], range);                                                       \
    step1[31] = clamp_value(step0[24] + step0[31], range);                                                       \
    step1[32] = step0[32];                                                                                       \
    step1[33] = step0[33];                                                                                       \
    step1[34] = step0[34];                                                                                       \
    step1[35] = step0[35];                                                                                       \
    step1[36] = half_btf(-cospi[16], step0[36], cospi[48], step0[59], CosBits);                                  \
    step1[37] = half_btf(-cospi[16], step0[37], cospi[48], step0[58], CosBits);                                  \
    step1[38] = half_btf(-cospi[16], step0[38], cospi[48], step0[57], CosBits);                                  \
    step1[39] = half_btf(-cospi[16], step0[39], cospi[48], step0[56], CosBits);                                  \
    step1[40] = half_btf(-cospi[48], step0[40], -cospi[16], step0[55], CosBits);                                 \
    step1[41] = half_btf(-cospi[48], step0[41], -cospi[16], step0[54], CosBits);                                 \
    step1[42] = half_btf(-cospi[48], step0[42], -cospi[16], step0[53], CosBits);                                 \
    step1[43] = half_btf(-cospi[48], step0[43], -cospi[16], step0[52], CosBits);                                 \
    step1[44] = step0[44];                                                                                       \
    step1[45] = step0[45];                                                                                       \
    step1[46] = step0[46];                                                                                       \
    step1[47] = step0[47];                                                                                       \
    step1[48] = step0[48];                                                                                       \
    step1[49] = step0[49];                                                                                       \
    step1[50] = step0[50];                                                                                       \
    step1[51] = step0[51];                                                                                       \
    step1[52] = half_btf(-cospi[16], step0[43], cospi[48], step0[52], CosBits);                                  \
    step1[53] = half_btf(-cospi[16], step0[42], cospi[48], step0[53], CosBits);                                  \
    step1[54] = half_btf(-cospi[16], step0[41], cospi[48], step0[54], CosBits);                                  \
    step1[55] = half_btf(-cospi[16], step0[40], cospi[48], step0[55], CosBits);                                  \
    step1[56] = half_btf(cospi[48], step0[39], cospi[16], step0[56], CosBits);                                   \
    step1[57] = half_btf(cospi[48], step0[38], cospi[16], step0[57], CosBits);                                   \
    step1[58] = half_btf(cospi[48], step0[37], cospi[16], step0[58], CosBits);                                   \
    step1[59] = half_btf(cospi[48], step0[36], cospi[16], step0[59], CosBits);                                   \
    step1[60] = step0[60];                                                                                       \
    step1[61] = step0[61];                                                                                       \
    step1[62] = step0[62];                                                                                       \
    step1[63] = step0[63];                                                                                       \
    step0[0] = clamp_value(step1[0] + step1[15], range);                                                         \
    step0[1] = clamp_value(step1[1] + step1[14], range);                                                         \
    step0[2] = clamp_value(step1[2] + step1[13], range);                                                         \
    step0[3] = clamp_value(step1[3] + step1[12], range);                                                         \
    step0[4] = clamp_value(step1[4] + step1[11], range);                                                         \
    step0[5] = clamp_value(step1[5] + step1[10], range);                                                         \
    step0[6] = clamp_value(step1[6] + step1[9], range);                                                          \
    step0[7] = clamp_value(step1[7] + step1[8], range);                                                          \
    step0[8] = clamp_value(step1[7] - step1[8], range);                                                          \
    step0[9] = clamp_value(step1[6] - step1[9], range);                                                          \
    step0[10] = clamp_value(step1[5] - step1[10], range);                                                        \
    step0[11] = clamp_value(step1[4] - step1[11], range);                                                        \
    step0[12] = clamp_value(step1[3] - step1[12], range);                                                        \
    step0[13] = clamp_value(step1[2] - step1[13], range);                                                        \
    step0[14] = clamp_value(step1[1] - step1[14], range);                                                        \
    step0[15] = clamp_value(step1[0] - step1[15], range);                                                        \
    step0[16] = step1[16];                                                                                       \
    step0[17] = step1[17];                                                                                       \
    step0[18] = step1[18];                                                                                       \
    step0[19] = step1[19];                                                                                       \
    step0[20] = half_btf(-cospi[32], step1[20], cospi[32], step1[27], CosBits);                                  \
    step0[21] = half_btf(-cospi[32], step1[21], cospi[32], step1[26], CosBits);                                  \
    step0[22] = half_btf(-cospi[32], step1[22], cospi[32], step1[25], CosBits);                                  \
    step0[23] = half_btf(-cospi[32], step1[23], cospi[32], step1[24], CosBits);                                  \
    step0[24] = half_btf(cospi[32], step1[23], cospi[32], step1[24], CosBits);                                   \
    step0[25] = half_btf(cospi[32], step1[22], cospi[32], step1[25], CosBits);                                   \
    step0[26] = half_btf(cospi[32], step1[21], cospi[32], step1[26], CosBits);                                   \
    step0[27] = half_btf(cospi[32], step1[20], cospi[32], step1[27], CosBits);                                   \
    step0[28] = step1[28];                                                                                       \
    step0[29] = step1[29];                                                                                       \
    step0[30] = step1[30];                                                                                       \
    step0[31] = step1[31];                                                                                       \
    step0[32] = clamp_value(step1[32] + step1[47], range);                                                       \
    step0[33] = clamp_value(step1[33] + step1[46], range);                                                       \
    step0[34] = clamp_value(step1[34] + step1[45], range);                                                       \
    step0[35] = clamp_value(step1[35] + step1[44], range);                                                       \
    step0[36] = clamp_value(step1[36] + step1[43], range);                                                       \
    step0[37] = clamp_value(step1[37] + step1[42], range);                                                       \
    step0[38] = clamp_value(step1[38] + step1[41], range);                                                       \
    step0[39] = clamp_value(step1[39] + step1[40], range);                                                       \
    step0[40] = clamp_value(step1[39] - step1[40], range);                                                       \
    step0[41] = clamp_value(step1[38] - step1[41], range);                                                       \
    step0[42] = clamp_value(step1[37] - step1[42], range);                                                       \
    step0[43] = clamp_value(step1[36] - step1[43], range);                                                       \
    step0[44] = clamp_value(step1[35] - step1[44], range);                                                       \
    step0[45] = clamp_value(step1[34] - step1[45], range);                                                       \
    step0[46] = clamp_value(step1[33] - step1[46], range);                                                       \
    step0[47] = clamp_value(step1[32] - step1[47], range);                                                       \
    step0[48] = clamp_value(-step1[48] + step1[63], range);                                                      \
    step0[49] = clamp_value(-step1[49] + step1[62], range);                                                      \
    step0[50] = clamp_value(-step1[50] + step1[61], range);                                                      \
    step0[51] = clamp_value(-step1[51] + step1[60], range);                                                      \
    step0[52] = clamp_value(-step1[52] + step1[59], range);                                                      \
    step0[53] = clamp_value(-step1[53] + step1[58], range);                                                      \
    step0[54] = clamp_value(-step1[54] + step1[57], range);                                                      \
    step0[55] = clamp_value(-step1[55] + step1[56], range);                                                      \
    step0[56] = clamp_value(step1[55] + step1[56], range);                                                       \
    step0[57] = clamp_value(step1[54] + step1[57], range);                                                       \
    step0[58] = clamp_value(step1[53] + step1[58], range);                                                       \
    step0[59] = clamp_value(step1[52] + step1[59], range);                                                       \
    step0[60] = clamp_value(step1[51] + step1[60], range);                                                       \
    step0[61] = clamp_value(step1[50] + step1[61], range);                                                       \
    step0[62] = clamp_value(step1[49] + step1[62], range);                                                       \
    step0[63] = clamp_value(step1[48] + step1[63], range);                                                       \
    step1[0] = clamp_value(step0[0] + step0[31], range);                                                         \
    step1[1] = clamp_value(step0[1] + step0[30], range);                                                         \
    step1[2] = clamp_value(step0[2] + step0[29], range);                                                         \
    step1[3] = clamp_value(step0[3] + step0[28], range);                                                         \
    step1[4] = clamp_value(step0[4] + step0[27], range);                                                         \
    step1[5] = clamp_value(step0[5] + step0[26], range);                                                         \
    step1[6] = clamp_value(step0[6] + step0[25], range);                                                         \
    step1[7] = clamp_value(step0[7] + step0[24], range);                                                         \
    step1[8] = clamp_value(step0[8] + step0[23], range);                                                         \
    step1[9] = clamp_value(step0[9] + step0[22], range);                                                         \
    step1[10] = clamp_value(step0[10] + step0[21], range);                                                       \
    step1[11] = clamp_value(step0[11] + step0[20], range);                                                       \
    step1[12] = clamp_value(step0[12] + step0[19], range);                                                       \
    step1[13] = clamp_value(step0[13] + step0[18], range);                                                       \
    step1[14] = clamp_value(step0[14] + step0[17], range);                                                       \
    step1[15] = clamp_value(step0[15] + step0[16], range);                                                       \
    step1[16] = clamp_value(step0[15] - step0[16], range);                                                       \
    step1[17] = clamp_value(step0[14] - step0[17], range);                                                       \
    step1[18] = clamp_value(step0[13] - step0[18], range);                                                       \
    step1[19] = clamp_value(step0[12] - step0[19], range);                                                       \
    step1[20] = clamp_value(step0[11] - step0[20], range);                                                       \
    step1[21] = clamp_value(step0[10] - step0[21], range);                                                       \
    step1[22] = clamp_value(step0[9] - step0[22], range);                                                        \
    step1[23] = clamp_value(step0[8] - step0[23], range);                                                        \
    step1[24] = clamp_value(step0[7] - step0[24], range);                                                        \
    step1[25] = clamp_value(step0[6] - step0[25], range);                                                        \
    step1[26] = clamp_value(step0[5] - step0[26], range);                                                        \
    step1[27] = clamp_value(step0[4] - step0[27], range);                                                        \
    step1[28] = clamp_value(step0[3] - step0[28], range);                                                        \
    step1[29] = clamp_value(step0[2] - step0[29], range);                                                        \
    step1[30] = clamp_value(step0[1] - step0[30], range);                                                        \
    step1[31] = clamp_value(step0[0] - step0[31], range);                                                        \
    step1[32] = step0[32];                                                                                       \
    step1[33] = step0[33];                                                                                       \
    step1[34] = step0[34];                                                                                       \
    step1[35] = step0[35];                                                                                       \
    step1[36] = step0[36];                                                                                       \
    step1[37] = step0[37];                                                                                       \
    step1[38] = step0[38];                                                                                       \
    step1[39] = step0[39];                                                                                       \
    step1[40] = half_btf(-cospi[32], step0[40], cospi[32], step0[55], CosBits);                                  \
    step1[41] = half_btf(-cospi[32], step0[41], cospi[32], step0[54], CosBits);                                  \
    step1[42] = half_btf(-cospi[32], step0[42], cospi[32], step0[53], CosBits);                                  \
    step1[43] = half_btf(-cospi[32], step0[43], cospi[32], step0[52], CosBits);                                  \
    step1[44] = half_btf(-cospi[32], step0[44], cospi[32], step0[51], CosBits);                                  \
    step1[45] = half_btf(-cospi[32], step0[45], cospi[32], step0[50], CosBits);                                  \
    step1[46] = half_btf(-cospi[32], step0[46], cospi[32], step0[49], CosBits);                                  \
    step1[47] = half_btf(-cospi[32], step0[47], cospi[32], step0[48], CosBits);                                  \
    step1[48] = half_btf(cospi[32], step0[47], cospi[32], step0[48], CosBits);                                   \
    step1[49] = half_btf(cospi[32], step0[46], cospi[32], step0[49], CosBits);                                   \
    step1[50] = half_btf(cospi[32], step0[45], cospi[32], step0[50], CosBits);                                   \
    step1[51] = half_btf(cospi[32], step0[44], cospi[32], step0[51], CosBits);                                   \
    step1[52] = half_btf(cospi[32], step0[43], cospi[32], step0[52], CosBits);                                   \
    step1[53] = half_btf(cospi[32], step0[42], cospi[32], step0[53], CosBits);                                   \
    step1[54] = half_btf(cospi[32], step0[41], cospi[32], step0[54], CosBits);                                   \
    step1[55] = half_btf(cospi[32], step0[40], cospi[32], step0[55], CosBits);                                   \
    step1[56] = step0[56];                                                                                       \
    step1[57] = step0[57];                                                                                       \
    step1[58] = step0[58];                                                                                       \
    step1[59] = step0[59];                                                                                       \
    step1[60] = step0[60];                                                                                       \
    step1[61] = step0[61];                                                                                       \
    step1[62] = step0[62];                                                                                       \
    step1[63] = step0[63];                                                                                       \
    output[out_offset + (out_stride)*0] = round_shift(clamp_value(step1[0] + step1[63], range), output_shift);   \
    output[out_offset + (out_stride)*1] = round_shift(clamp_value(step1[1] + step1[62], range), output_shift);   \
    output[out_offset + (out_stride)*2] = round_shift(clamp_value(step1[2] + step1[61], range), output_shift);   \
    output[out_offset + (out_stride)*3] = round_shift(clamp_value(step1[3] + step1[60], range), output_shift);   \
    output[out_offset + (out_stride)*4] = round_shift(clamp_value(step1[4] + step1[59], range), output_shift);   \
    output[out_offset + (out_stride)*5] = round_shift(clamp_value(step1[5] + step1[58], range), output_shift);   \
    output[out_offset + (out_stride)*6] = round_shift(clamp_value(step1[6] + step1[57], range), output_shift);   \
    output[out_offset + (out_stride)*7] = round_shift(clamp_value(step1[7] + step1[56], range), output_shift);   \
    output[out_offset + (out_stride)*8] = round_shift(clamp_value(step1[8] + step1[55], range), output_shift);   \
    output[out_offset + (out_stride)*9] = round_shift(clamp_value(step1[9] + step1[54], range), output_shift);   \
    output[out_offset + (out_stride)*10] = round_shift(clamp_value(step1[10] + step1[53], range), output_shift); \
    output[out_offset + (out_stride)*11] = round_shift(clamp_value(step1[11] + step1[52], range), output_shift); \
    output[out_offset + (out_stride)*12] = round_shift(clamp_value(step1[12] + step1[51], range), output_shift); \
    output[out_offset + (out_stride)*13] = round_shift(clamp_value(step1[13] + step1[50], range), output_shift); \
    output[out_offset + (out_stride)*14] = round_shift(clamp_value(step1[14] + step1[49], range), output_shift); \
    output[out_offset + (out_stride)*15] = round_shift(clamp_value(step1[15] + step1[48], range), output_shift); \
    output[out_offset + (out_stride)*16] = round_shift(clamp_value(step1[16] + step1[47], range), output_shift); \
    output[out_offset + (out_stride)*17] = round_shift(clamp_value(step1[17] + step1[46], range), output_shift); \
    output[out_offset + (out_stride)*18] = round_shift(clamp_value(step1[18] + step1[45], range), output_shift); \
    output[out_offset + (out_stride)*19] = round_shift(clamp_value(step1[19] + step1[44], range), output_shift); \
    output[out_offset + (out_stride)*20] = round_shift(clamp_value(step1[20] + step1[43], range), output_shift); \
    output[out_offset + (out_stride)*21] = round_shift(clamp_value(step1[21] + step1[42], range), output_shift); \
    output[out_offset + (out_stride)*22] = round_shift(clamp_value(step1[22] + step1[41], range), output_shift); \
    output[out_offset + (out_stride)*23] = round_shift(clamp_value(step1[23] + step1[40], range), output_shift); \
    output[out_offset + (out_stride)*24] = round_shift(clamp_value(step1[24] + step1[39], range), output_shift); \
    output[out_offset + (out_stride)*25] = round_shift(clamp_value(step1[25] + step1[38], range), output_shift); \
    output[out_offset + (out_stride)*26] = round_shift(clamp_value(step1[26] + step1[37], range), output_shift); \
    output[out_offset + (out_stride)*27] = round_shift(clamp_value(step1[27] + step1[36], range), output_shift); \
    output[out_offset + (out_stride)*28] = round_shift(clamp_value(step1[28] + step1[35], range), output_shift); \
    output[out_offset + (out_stride)*29] = round_shift(clamp_value(step1[29] + step1[34], range), output_shift); \
    output[out_offset + (out_stride)*30] = round_shift(clamp_value(step1[30] + step1[33], range), output_shift); \
    output[out_offset + (out_stride)*31] = round_shift(clamp_value(step1[31] + step1[32], range), output_shift); \
    output[out_offset + (out_stride)*32] = round_shift(clamp_value(step1[31] - step1[32], range), output_shift); \
    output[out_offset + (out_stride)*33] = round_shift(clamp_value(step1[30] - step1[33], range), output_shift); \
    output[out_offset + (out_stride)*34] = round_shift(clamp_value(step1[29] - step1[34], range), output_shift); \
    output[out_offset + (out_stride)*35] = round_shift(clamp_value(step1[28] - step1[35], range), output_shift); \
    output[out_offset + (out_stride)*36] = round_shift(clamp_value(step1[27] - step1[36], range), output_shift); \
    output[out_offset + (out_stride)*37] = round_shift(clamp_value(step1[26] - step1[37], range), output_shift); \
    output[out_offset + (out_stride)*38] = round_shift(clamp_value(step1[25] - step1[38], range), output_shift); \
    output[out_offset + (out_stride)*39] = round_shift(clamp_value(step1[24] - step1[39], range), output_shift); \
    output[out_offset + (out_stride)*40] = round_shift(clamp_value(step1[23] - step1[40], range), output_shift); \
    output[out_offset + (out_stride)*41] = round_shift(clamp_value(step1[22] - step1[41], range), output_shift); \
    output[out_offset + (out_stride)*42] = round_shift(clamp_value(step1[21] - step1[42], range), output_shift); \
    output[out_offset + (out_stride)*43] = round_shift(clamp_value(step1[20] - step1[43], range), output_shift); \
    output[out_offset + (out_stride)*44] = round_shift(clamp_value(step1[19] - step1[44], range), output_shift); \
    output[out_offset + (out_stride)*45] = round_shift(clamp_value(step1[18] - step1[45], range), output_shift); \
    output[out_offset + (out_stride)*46] = round_shift(clamp_value(step1[17] - step1[46], range), output_shift); \
    output[out_offset + (out_stride)*47] = round_shift(clamp_value(step1[16] - step1[47], range), output_shift); \
    output[out_offset + (out_stride)*48] = round_shift(clamp_value(step1[15] - step1[48], range), output_shift); \
    output[out_offset + (out_stride)*49] = round_shift(clamp_value(step1[14] - step1[49], range), output_shift); \
    output[out_offset + (out_stride)*50] = round_shift(clamp_value(step1[13] - step1[50], range), output_shift); \
    output[out_offset + (out_stride)*51] = round_shift(clamp_value(step1[12] - step1[51], range), output_shift); \
    output[out_offset + (out_stride)*52] = round_shift(clamp_value(step1[11] - step1[52], range), output_shift); \
    output[out_offset + (out_stride)*53] = round_shift(clamp_value(step1[10] - step1[53], range), output_shift); \
    output[out_offset + (out_stride)*54] = round_shift(clamp_value(step1[9] - step1[54], range), output_shift);  \
    output[out_offset + (out_stride)*55] = round_shift(clamp_value(step1[8] - step1[55], range), output_shift);  \
    output[out_offset + (out_stride)*56] = round_shift(clamp_value(step1[7] - step1[56], range), output_shift);  \
    output[out_offset + (out_stride)*57] = round_shift(clamp_value(step1[6] - step1[57], range), output_shift);  \
    output[out_offset + (out_stride)*58] = round_shift(clamp_value(step1[5] - step1[58], range), output_shift);  \
    output[out_offset + (out_stride)*59] = round_shift(clamp_value(step1[4] - step1[59], range), output_shift);  \
    output[out_offset + (out_stride)*60] = round_shift(clamp_value(step1[3] - step1[60], range), output_shift);  \
    output[out_offset + (out_stride)*61] = round_shift(clamp_value(step1[2] - step1[61], range), output_shift);  \
    output[out_offset + (out_stride)*62] = round_shift(clamp_value(step1[1] - step1[62], range), output_shift);  \
    output[out_offset + (out_stride)*63] = round_shift(clamp_value(step1[0] - step1[63], range), output_shift);  \
  }

#define IIDENTITY_N(N, mult, shift, input, in_offset, in_stride, output, out_offset, out_stride, range, output_shift) \
  {                                                                                                                   \
    for (int i = 0; i < N; ++i) {                                                                                     \
      output[out_offset + i * (out_stride)] =                                                                         \
          clamp_value(round_shift(round_shift(mult * clamp_value(input[in_offset + i * (in_stride)], range), shift),  \
                                  output_shift),                                                                      \
                      range);                                                                                         \
    }                                                                                                                 \
  }
