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

#include "inter_common.h"

#define SubblockW 4
#define SubblockH 4
#define VertBits 7
#define VertSumAdd ((1 << 19) + (1 << (VertBits - 1)))
#define VertSub ((1 << (19 - VertBits - 1)) + (1 << (19 - VertBits)))
#define RoundFinal 4
#define DiffWTDBase 38
#define DiffWTDRoundAdd 8
#define DiffWTDRoundShft 8
#define DiffWTDBits 6
#define DiffWTDMax 64

groupshared int4 mem[11 * 16];

int blend(int src0, int src1, int coef) {
  int result = (src0 * coef + src1 * (64 - coef)) >> 6;
  result = (result - VertSub + (1 << (RoundFinal - 1))) >> RoundFinal;
  return clamp(result, 0, 255);
}

[numthreads(64, 1, 1)] void main(uint3 thread
                                 : SV_DispatchThreadID) {
  // A thread behind the last block of the dispatch does nothing, but it takes part in the group syncs: a
  // group sync has to be reached by all threads of the group.
  const bool active = thread.x < cb_wi_count;

  const int w_log = cb_width_log2;
  const int h_log = cb_height_log2;
  int wi = thread.x & 3;
  const int subblock = low_bits(thread.x >> 2, w_log + h_log);
  int block_index = cb_pass_offset + (thread.x >> (w_log + h_log + 2));
  uint4 block = uint4(0, 0, 0, 0);
  [branch] if (active) block = pred_blocks.Load4(block_index * 16);

  int x = SubblockW * ((block.x & 0xffff) + low_bits(subblock, w_log));
  int y = SubblockH * ((block.x >> 16) + (subblock >> w_log));

  const int plane = block.y & 3;
  const int2 dims = cb_dims[plane > 0].xy;
  const int noskip = block.y & NoSkipFlag;
  const int subsampling = plane > 0;
  const int local_ofs = ((thread.x & 63) >> 2) * 11;

  // REF0
  int ref = (block.y >> 2) & 7;
  int refplane = ref * 3 + plane;
  int ref_offset = cb_refplanes[refplane].y;
  int ref_stride = cb_refplanes[refplane].x;
  int4 info0 = cb_gm_warp[ref].info0;
  int4 output0 = int4(0, 0, 0, 0);
  // The prediction from a reference is two passes: the four threads of a 4x4 filter its eleven lines
  // horizontally into the group memory, three lines or two each, then every thread filters eight of them
  // vertically. The threads of a group run in no particular order, so it takes a group sync to have all
  // lines there before any of them is read, and another to have all of them read before the memory is used
  // again; a group sync has to be reached by all threads of the group, so each pass is a branch of its own
  // and the syncs are between them. (Upstream had no barrier here at all: that held only where the 64
  // threads of a group ran in lockstep, as one wave.) What the vertical pass takes over from the
  // horizontal one:
  int4 info2 = int4(0, 0, 0, 0);
  int sy4 = 0;
  int filter_v = 0;

  if (active && info0.x) {
    int4 info1 = cb_gm_warp[ref].info1;
    info2 = cb_gm_warp[ref].info2;
    const int src_x = ((x & (~7)) + 4) << subsampling;
    const int src_y = ((y & (~7)) + 4) << subsampling;
    const int dst_x = info1.x * src_x + info1.y * src_y + (int)info0.z;
    const int dst_y = info1.z * src_x + info1.w * src_y + (int)info0.w;
    const int x4 = dst_x >> subsampling;
    const int y4 = dst_y >> subsampling;

    int ix4 = clamp((x4 >> WarpPrecBits) - 7 + (x & 7), -11, dims.x);
    int iy4 = (y4 >> WarpPrecBits) - 7 + (y & 7);

    int sx4 = x4 & ((1 << WarpPrecBits) - 1);
    sy4 = y4 & ((1 << WarpPrecBits) - 1);

    sx4 += info2.x * (-4) + info2.y * (-4);
    sy4 += info2.w * (-4) + info2.z * (-4);

    sx4 &= ~((1 << WarpReduceBits) - 1);
    sy4 &= ~((1 << WarpReduceBits) - 1);

    sx4 += info2.y * ((y & 7) - 3) + info2.x * (x & 7);
    sy4 += info2.z * (y & 7) + info2.w * (x & 7);

    ref_offset += ix4;

    for (int i = wi; i < 11; i += 4) {
      mem[local_ofs + i] = filter_line_warp(dst_frame, ref_offset + ref_stride * clamp(iy4 + i, 0, dims.y), warp_filter,
                                            sx4 + info2.y * i, info2.x);
    }
  } else if (active) {
    int mv = block.z;
    int mvx = x + ((mv) >> (16 + SUBPEL_BITS)) - 3;
    int mvy = y + ((mv << 16) >> (16 + SUBPEL_BITS)) - 3;
    mvx = clamp(mvx, -11, dims.x);
    int filter_h = (((block.y >> 5) & 15) << 4) + ((mv >> 16) & SUBPEL_MASK);
    filter_v = (((block.y >> 9) & 15) << 4) + (mv & SUBPEL_MASK);
    int4 kernel_h0 = cb_kernels[filter_h][0];
    int4 kernel_h1 = cb_kernels[filter_h][1];

    for (int i = wi; i < 11; i += 4) {
      mem[local_ofs + i] =
          filter_line(dst_frame, ref_offset + mvx + ref_stride * clamp(mvy + i, 0, dims.y), kernel_h0, kernel_h1);
    }
  }
  GroupMemoryBarrierWithGroupSync();
  if (active && info0.x) {
    int sy = sy4 + wi * info2.z;

    int filter_addr;
    int4 filter0, filter1;

    filter_addr = WarpFilterSize * (((sy + 0 * info2.w + WarpFiltRoundAdd) >> WarpFiltRoundBits) + WarpFiltOffset);
    filter0 = warp_filter.Load4(filter_addr);
    filter1 = warp_filter.Load4(filter_addr + 16);
    output0.x = mem[local_ofs + wi + 0].x * filter0.x + mem[local_ofs + wi + 1].x * filter0.y +
                mem[local_ofs + wi + 2].x * filter0.z + mem[local_ofs + wi + 3].x * filter0.w +
                mem[local_ofs + wi + 4].x * filter1.x + mem[local_ofs + wi + 5].x * filter1.y +
                mem[local_ofs + wi + 6].x * filter1.z + mem[local_ofs + wi + 7].x * filter1.w;

    filter_addr = WarpFilterSize * (((sy + 1 * info2.w + WarpFiltRoundAdd) >> WarpFiltRoundBits) + WarpFiltOffset);
    filter0 = warp_filter.Load4(filter_addr);
    filter1 = warp_filter.Load4(filter_addr + 16);
    output0.y = mem[local_ofs + wi + 0].y * filter0.x + mem[local_ofs + wi + 1].y * filter0.y +
                mem[local_ofs + wi + 2].y * filter0.z + mem[local_ofs + wi + 3].y * filter0.w +
                mem[local_ofs + wi + 4].y * filter1.x + mem[local_ofs + wi + 5].y * filter1.y +
                mem[local_ofs + wi + 6].y * filter1.z + mem[local_ofs + wi + 7].y * filter1.w;

    filter_addr = WarpFilterSize * (((sy + 2 * info2.w + WarpFiltRoundAdd) >> WarpFiltRoundBits) + WarpFiltOffset);
    filter0 = warp_filter.Load4(filter_addr);
    filter1 = warp_filter.Load4(filter_addr + 16);
    output0.z = mem[local_ofs + wi + 0].z * filter0.x + mem[local_ofs + wi + 1].z * filter0.y +
                mem[local_ofs + wi + 2].z * filter0.z + mem[local_ofs + wi + 3].z * filter0.w +
                mem[local_ofs + wi + 4].z * filter1.x + mem[local_ofs + wi + 5].z * filter1.y +
                mem[local_ofs + wi + 6].z * filter1.z + mem[local_ofs + wi + 7].z * filter1.w;

    filter_addr = WarpFilterSize * (((sy + 3 * info2.w + WarpFiltRoundAdd) >> WarpFiltRoundBits) + WarpFiltOffset);
    filter0 = warp_filter.Load4(filter_addr);
    filter1 = warp_filter.Load4(filter_addr + 16);
    output0.w = mem[local_ofs + wi + 0].w * filter0.x + mem[local_ofs + wi + 1].w * filter0.y +
                mem[local_ofs + wi + 2].w * filter0.z + mem[local_ofs + wi + 3].w * filter0.w +
                mem[local_ofs + wi + 4].w * filter1.x + mem[local_ofs + wi + 5].w * filter1.y +
                mem[local_ofs + wi + 6].w * filter1.z + mem[local_ofs + wi + 7].w * filter1.w;
  } else if (active) {
    int4 kernel_v0 = cb_kernels[filter_v][0];
    int4 kernel_v1 = cb_kernels[filter_v][1];

    output0.x = mem[local_ofs + wi + 0].x * kernel_v0.x + mem[local_ofs + wi + 1].x * kernel_v0.y +
                mem[local_ofs + wi + 2].x * kernel_v0.z + mem[local_ofs + wi + 3].x * kernel_v0.w +
                mem[local_ofs + wi + 4].x * kernel_v1.x + mem[local_ofs + wi + 5].x * kernel_v1.y +
                mem[local_ofs + wi + 6].x * kernel_v1.z + mem[local_ofs + wi + 7].x * kernel_v1.w;
    output0.y = mem[local_ofs + wi + 0].y * kernel_v0.x + mem[local_ofs + wi + 1].y * kernel_v0.y +
                mem[local_ofs + wi + 2].y * kernel_v0.z + mem[local_ofs + wi + 3].y * kernel_v0.w +
                mem[local_ofs + wi + 4].y * kernel_v1.x + mem[local_ofs + wi + 5].y * kernel_v1.y +
                mem[local_ofs + wi + 6].y * kernel_v1.z + mem[local_ofs + wi + 7].y * kernel_v1.w;
    output0.z = mem[local_ofs + wi + 0].z * kernel_v0.x + mem[local_ofs + wi + 1].z * kernel_v0.y +
                mem[local_ofs + wi + 2].z * kernel_v0.z + mem[local_ofs + wi + 3].z * kernel_v0.w +
                mem[local_ofs + wi + 4].z * kernel_v1.x + mem[local_ofs + wi + 5].z * kernel_v1.y +
                mem[local_ofs + wi + 6].z * kernel_v1.z + mem[local_ofs + wi + 7].z * kernel_v1.w;
    output0.w = mem[local_ofs + wi + 0].w * kernel_v0.x + mem[local_ofs + wi + 1].w * kernel_v0.y +
                mem[local_ofs + wi + 2].w * kernel_v0.z + mem[local_ofs + wi + 3].w * kernel_v0.w +
                mem[local_ofs + wi + 4].w * kernel_v1.x + mem[local_ofs + wi + 5].w * kernel_v1.y +
                mem[local_ofs + wi + 6].w * kernel_v1.z + mem[local_ofs + wi + 7].w * kernel_v1.w;
  }
  GroupMemoryBarrierWithGroupSync();

  // REF1
  ref = (block.y >> 14) & 7;
  refplane = ref * 3 + plane;
  ref_offset = cb_refplanes[refplane].y;
  ref_stride = cb_refplanes[refplane].x;
  info0 = cb_gm_warp[ref].info0;

  int4 output1 = int4(0, 0, 0, 0);
  if (active && info0.x) {
    int4 info1 = cb_gm_warp[ref].info1;
    info2 = cb_gm_warp[ref].info2;
    const int src_x = ((x & (~7)) + 4) << subsampling;
    const int src_y = ((y & (~7)) + 4) << subsampling;
    const int dst_x = info1.x * src_x + info1.y * src_y + (int)info0.z;
    const int dst_y = info1.z * src_x + info1.w * src_y + (int)info0.w;
    const int x4 = dst_x >> subsampling;
    const int y4 = dst_y >> subsampling;

    int ix4 = clamp((x4 >> WarpPrecBits) - 7 + (x & 7), -11, dims.x);
    int iy4 = (y4 >> WarpPrecBits) - 7 + (y & 7);

    int sx4 = x4 & ((1 << WarpPrecBits) - 1);
    sy4 = y4 & ((1 << WarpPrecBits) - 1);

    sx4 += info2.x * (-4) + info2.y * (-4);
    sy4 += info2.w * (-4) + info2.z * (-4);

    sx4 &= ~((1 << WarpReduceBits) - 1);
    sy4 &= ~((1 << WarpReduceBits) - 1);

    sx4 += info2.y * ((y & 7) - 3) + info2.x * (x & 7);
    sy4 += info2.z * (y & 7) + info2.w * (x & 7);

    ref_offset += ix4;

    for (int i = wi; i < 11; i += 4) {
      mem[local_ofs + i] = filter_line_warp(dst_frame, ref_offset + ref_stride * clamp(iy4 + i, 0, dims.y), warp_filter,
                                            sx4 + info2.y * i, info2.x);
    }
  } else if (active) {
    int mv = block.w;
    int mvx = x + ((mv) >> (16 + SUBPEL_BITS)) - 3;
    int mvy = y + ((mv << 16) >> (16 + SUBPEL_BITS)) - 3;
    mvx = clamp(mvx, -11, dims.x);

    int filter_h = (((block.y >> 5) & 15) << 4) + ((mv >> 16) & SUBPEL_MASK);
    filter_v = (((block.y >> 9) & 15) << 4) + (mv & SUBPEL_MASK);
    int4 kernel_h0 = cb_kernels[filter_h][0];
    int4 kernel_h1 = cb_kernels[filter_h][1];

    for (int i = wi; i < 11; i += 4) {
      mem[local_ofs + i] =
          filter_line(dst_frame, ref_offset + mvx + ref_stride * clamp(mvy + i, 0, dims.y), kernel_h0, kernel_h1);
    }
  }
  GroupMemoryBarrierWithGroupSync();
  if (active && info0.x) {
    int sy = sy4 + wi * info2.z;
    int filter_addr;
    int4 filter0, filter1;

    filter_addr = WarpFilterSize * (((sy + 0 * info2.w + WarpFiltRoundAdd) >> WarpFiltRoundBits) + WarpFiltOffset);
    filter0 = warp_filter.Load4(filter_addr);
    filter1 = warp_filter.Load4(filter_addr + 16);
    output1.x = mem[local_ofs + wi + 0].x * filter0.x + mem[local_ofs + wi + 1].x * filter0.y +
                mem[local_ofs + wi + 2].x * filter0.z + mem[local_ofs + wi + 3].x * filter0.w +
                mem[local_ofs + wi + 4].x * filter1.x + mem[local_ofs + wi + 5].x * filter1.y +
                mem[local_ofs + wi + 6].x * filter1.z + mem[local_ofs + wi + 7].x * filter1.w;

    filter_addr = WarpFilterSize * (((sy + 1 * info2.w + WarpFiltRoundAdd) >> WarpFiltRoundBits) + WarpFiltOffset);
    filter0 = warp_filter.Load4(filter_addr);
    filter1 = warp_filter.Load4(filter_addr + 16);
    output1.y = mem[local_ofs + wi + 0].y * filter0.x + mem[local_ofs + wi + 1].y * filter0.y +
                mem[local_ofs + wi + 2].y * filter0.z + mem[local_ofs + wi + 3].y * filter0.w +
                mem[local_ofs + wi + 4].y * filter1.x + mem[local_ofs + wi + 5].y * filter1.y +
                mem[local_ofs + wi + 6].y * filter1.z + mem[local_ofs + wi + 7].y * filter1.w;

    filter_addr = WarpFilterSize * (((sy + 2 * info2.w + WarpFiltRoundAdd) >> WarpFiltRoundBits) + WarpFiltOffset);
    filter0 = warp_filter.Load4(filter_addr);
    filter1 = warp_filter.Load4(filter_addr + 16);
    output1.z = mem[local_ofs + wi + 0].z * filter0.x + mem[local_ofs + wi + 1].z * filter0.y +
                mem[local_ofs + wi + 2].z * filter0.z + mem[local_ofs + wi + 3].z * filter0.w +
                mem[local_ofs + wi + 4].z * filter1.x + mem[local_ofs + wi + 5].z * filter1.y +
                mem[local_ofs + wi + 6].z * filter1.z + mem[local_ofs + wi + 7].z * filter1.w;

    filter_addr = WarpFilterSize * (((sy + 3 * info2.w + WarpFiltRoundAdd) >> WarpFiltRoundBits) + WarpFiltOffset);
    filter0 = warp_filter.Load4(filter_addr);
    filter1 = warp_filter.Load4(filter_addr + 16);
    output1.w = mem[local_ofs + wi + 0].w * filter0.x + mem[local_ofs + wi + 1].w * filter0.y +
                mem[local_ofs + wi + 2].w * filter0.z + mem[local_ofs + wi + 3].w * filter0.w +
                mem[local_ofs + wi + 4].w * filter1.x + mem[local_ofs + wi + 5].w * filter1.y +
                mem[local_ofs + wi + 6].w * filter1.z + mem[local_ofs + wi + 7].w * filter1.w;
  } else if (active) {
    int4 kernel_v0 = cb_kernels[filter_v][0];
    int4 kernel_v1 = cb_kernels[filter_v][1];

    output1.x = mem[local_ofs + wi + 0].x * kernel_v0.x + mem[local_ofs + wi + 1].x * kernel_v0.y +
                mem[local_ofs + wi + 2].x * kernel_v0.z + mem[local_ofs + wi + 3].x * kernel_v0.w +
                mem[local_ofs + wi + 4].x * kernel_v1.x + mem[local_ofs + wi + 5].x * kernel_v1.y +
                mem[local_ofs + wi + 6].x * kernel_v1.z + mem[local_ofs + wi + 7].x * kernel_v1.w;
    output1.y = mem[local_ofs + wi + 0].y * kernel_v0.x + mem[local_ofs + wi + 1].y * kernel_v0.y +
                mem[local_ofs + wi + 2].y * kernel_v0.z + mem[local_ofs + wi + 3].y * kernel_v0.w +
                mem[local_ofs + wi + 4].y * kernel_v1.x + mem[local_ofs + wi + 5].y * kernel_v1.y +
                mem[local_ofs + wi + 6].y * kernel_v1.z + mem[local_ofs + wi + 7].y * kernel_v1.w;
    output1.z = mem[local_ofs + wi + 0].z * kernel_v0.x + mem[local_ofs + wi + 1].z * kernel_v0.y +
                mem[local_ofs + wi + 2].z * kernel_v0.z + mem[local_ofs + wi + 3].z * kernel_v0.w +
                mem[local_ofs + wi + 4].z * kernel_v1.x + mem[local_ofs + wi + 5].z * kernel_v1.y +
                mem[local_ofs + wi + 6].z * kernel_v1.z + mem[local_ofs + wi + 7].z * kernel_v1.w;
    output1.w = mem[local_ofs + wi + 0].w * kernel_v0.x + mem[local_ofs + wi + 1].w * kernel_v0.y +
                mem[local_ofs + wi + 2].w * kernel_v0.z + mem[local_ofs + wi + 3].w * kernel_v0.w +
                mem[local_ofs + wi + 4].w * kernel_v1.x + mem[local_ofs + wi + 5].w * kernel_v1.y +
                mem[local_ofs + wi + 6].w * kernel_v1.z + mem[local_ofs + wi + 7].w * kernel_v1.w;
  }

  output0.x = (output0.x + VertSumAdd) >> VertBits;
  output0.y = (output0.y + VertSumAdd) >> VertBits;
  output0.z = (output0.z + VertSumAdd) >> VertBits;
  output0.w = (output0.w + VertSumAdd) >> VertBits;
  output1.x = (output1.x + VertSumAdd) >> VertBits;
  output1.y = (output1.y + VertSumAdd) >> VertBits;
  output1.z = (output1.z + VertSumAdd) >> VertBits;
  output1.w = (output1.w + VertSumAdd) >> VertBits;

  y += wi;
  const int output_stride = cb_planes[plane].x;
  const int output_offset = cb_planes[plane].y + x + y * output_stride;

  int compound_type = block.y >> 30;
  int4 coefs;
  if (compound_type == 0) {
    coefs.x = ((block.y >> 17) & 15) << 2;
    coefs.yzw = coefs.xxx;
  } else if (compound_type == 1) {
    int wedge_stride = SubblockW << w_log;
    int wedge_addr = (((block.y >> 17) & 0x1fff) << 6) + SubblockW * low_bits(subblock, w_log) +
                     (SubblockH * (subblock >> w_log) + wi) * wedge_stride;
    uint wedge = comp_mask.Load(wedge_addr);
    coefs.x = (wedge >> 0) & 255;
    coefs.y = (wedge >> 8) & 255;
    coefs.z = (wedge >> 16) & 255;
    coefs.w = (wedge >> 24) & 255;
  } else if (compound_type == 3) {
    uint m = dst_frame.Load(output_offset);
    coefs.x = (m >> 0) & 255;
    coefs.y = (m >> 8) & 255;
    coefs.z = (m >> 16) & 255;
    coefs.w = (m >> 24) & 255;
  } else {
    coefs.x = clamp(DiffWTDBase + ((abs(output0.x - output1.x) + DiffWTDRoundAdd) >> DiffWTDRoundShft), 0, DiffWTDMax);
    coefs.y = clamp(DiffWTDBase + ((abs(output0.y - output1.y) + DiffWTDRoundAdd) >> DiffWTDRoundShft), 0, DiffWTDMax);
    coefs.z = clamp(DiffWTDBase + ((abs(output0.z - output1.z) + DiffWTDRoundAdd) >> DiffWTDRoundShft), 0, DiffWTDMax);
    coefs.w = clamp(DiffWTDBase + ((abs(output0.w - output1.w) + DiffWTDRoundAdd) >> DiffWTDRoundShft), 0, DiffWTDMax);
    if ((block.y >> 17) & 1) coefs = int4(64, 64, 64, 64) - coefs;
  }

  output0.x = blend(output0.x, output1.x, coefs.x);
  output0.y = blend(output0.y, output1.y, coefs.y);
  output0.z = blend(output0.z, output1.z, coefs.z);
  output0.w = blend(output0.w, output1.w, coefs.w);

  if (noskip) {
    const int res_stride = cb_planes[plane].z;
    const int res_offset = cb_planes[plane].w;
    int2 r = (int2)residuals.Load2(res_offset + (x << 1) + y * res_stride);
    output0.x += (r.x << 16) >> 16;
    output0.y += r.x >> 16;
    output0.z += (r.y << 16) >> 16;
    output0.w += r.y >> 16;
    output0 = clamp(output0, 0, 255);
  }
  if (active) dst_frame.Store(output_offset, output0.x | (output0.y << 8) | (output0.z << 16) | (output0.w << 24));

  // The mask of a difference weighted block goes to its chroma planes at half the size. The four threads of
  // a 4x4 put the sums of their lines into the group memory, and one thread of every other 4x4 adds up its
  // own and its neighbour's: a group sync before the memory is used again, and one before the sums are
  // read.
  GroupMemoryBarrierWithGroupSync();
  if (active && compound_type == 2) {
    wi = thread.x & 63;
    coefs.x = coefs.x + coefs.y;
    coefs.y = coefs.z + coefs.w;
    mem[wi].xy = coefs.xy;
  }
  GroupMemoryBarrierWithGroupSync();
  if (active && compound_type == 2) {
    if ((wi & 5) == 0)  // filter odd cols and lines
    {
      coefs.xy += mem[wi + 1].xy;  // next line
      coefs.zw = mem[wi + 4].xy;   // next 4x4 col
      coefs.zw += mem[wi + 5].xy;  // next 4x4 col, next line
      coefs.x = (coefs.x + 2) >> 2;
      coefs.y = (coefs.y + 2) >> 2;
      coefs.z = (coefs.z + 2) >> 2;
      coefs.w = (coefs.w + 2) >> 2;

      int chroma_offset = (x + y * cb_planes[1].x) >> 1;
      uint mask = coefs.x | (coefs.y << 8) | (coefs.z << 16) | (coefs.w << 24);
      dst_frame.Store(cb_planes[1].y + chroma_offset, mask);
      dst_frame.Store(cb_planes[2].y + chroma_offset, mask);
    }
  }
}
