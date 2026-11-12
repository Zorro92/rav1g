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
#define DiffWTDRoundAdd (1 << 3)
#define DiffWTDRoundShft 8
#define DiffWTDBits 6
#define DiffWTDMax 64
#define LocalStride 20

groupshared int intermediate_buffer[64 * LocalStride];

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
  const int dx = SubblockW * low_bits(subblock, w_log);
  const int dy = SubblockH * (subblock >> w_log);
  const int mbx = SubblockW * (block.x & 0xffff);
  const int mby = SubblockH * (block.x >> 16);
  int x = mbx + dx;
  int y = mby + dy;

  const int plane = block.y & 3;
  const int noskip = block.y & NoSkipFlag;
  const int subsampling = plane > 0;
  const int local_ofs = (thread.x & 63) * LocalStride;

  // REF0
  int ref = (block.y >> 2) & 7;
  int refplane = ref * 3 + plane;
  int ref_offset = cb_refplanes[refplane].y;
  int ref_stride = cb_refplanes[refplane].x;
  int ref_w = cb_refplanes[refplane].z;
  int ref_h = cb_refplanes[refplane].w;
  int4 info0 = cb_gm_warp[ref].info0;
  int output0[4];
  int i;
  // The prediction from a reference is two passes: every thread filters the lines of its column
  // horizontally into the group memory, then filters four columns of its 4x4 vertically, which are those of
  // the other threads of the 4x4. The threads of a group run in no particular order, so it takes a group
  // sync to have all lines there before any of them is read, and another to have all of them read before
  // the memory is used again; a group sync has to be reached by all threads of the group, so each pass is a
  // branch of its own and the syncs are between them. (Upstream had plain group memory barriers inside the
  // branches, which make no thread wait: that held only where the 64 threads of a group ran in lockstep, as
  // one wave.) What the vertical pass takes over from the horizontal one:
  int4 info2 = int4(0, 0, 0, 0);
  int sy4 = 0;
  int4 scale = int4(0, 0, 0, 0);
  int mvy = 0;
  if (active && info0.x) {
    int4 info1 = cb_gm_warp[ref].info1;
    info2 = cb_gm_warp[ref].info2;
    const int src_x = ((x & (~7)) + 4) << subsampling;
    const int src_y = ((y & (~7)) + 4) << subsampling;
    const int dst_x = info1.x * src_x + info1.y * src_y + (int)info0.z;
    const int dst_y = info1.z * src_x + info1.w * src_y + (int)info0.w;
    const int x4 = dst_x >> subsampling;
    const int y4 = dst_y >> subsampling;

    int ix4 = clamp((x4 >> WarpPrecBits) - 7 + (x & 7), -11, ref_w);
    int iy4 = (y4 >> WarpPrecBits) - 7 + (y & 7);

    int sx4 = x4 & ((1 << WarpPrecBits) - 1);
    sy4 = y4 & ((1 << WarpPrecBits) - 1);

    sx4 += info2.x * (-4) + info2.y * (-4);
    sy4 += info2.w * (-4) + info2.z * (-4);

    sx4 &= ~((1 << WarpReduceBits) - 1);
    sy4 &= ~((1 << WarpReduceBits) - 1);

    sx4 += info2.y * ((y & 7) - 3) + info2.x * (x & 7);
    sy4 += info2.z * (y & 7) + info2.w * (x & 7);

    ref_offset += ix4 + wi;
    for (i = 0; i < 11; ++i) {
      const int offset = ref_offset + ref_stride * clamp(iy4 + i, 0, ref_h);
      uint3 l = dst_frame.Load3(offset & (~3));
      uint shift = (offset & 3) * 8;
      l.x = (l.x >> shift) | ((l.y << (24 - shift)) << 8);
      l.y = (l.y >> shift) | ((l.z << (24 - shift)) << 8);
      int sum = WarpHorizSumAdd;
      const int sx = sx4 + info2.y * i + wi * info2.x;
      int filter_offset = WarpFilterSize * (((sx + WarpFiltRoundAdd) >> WarpFiltRoundBits) + WarpFiltOffset);
      uint4 f0 = warp_filter.Load4(filter_offset);
      uint4 f1 = warp_filter.Load4(filter_offset + 16);
      sum += f0.x * (int)((l.x >> 0) & 0xff);
      sum += f0.y * (int)((l.x >> 8) & 0xff);
      sum += f0.z * (int)((l.x >> 16) & 0xff);
      sum += f0.w * (int)((l.x >> 24) & 0xff);
      sum += f1.x * (int)((l.y >> 0) & 0xff);
      sum += f1.y * (int)((l.y >> 8) & 0xff);
      sum += f1.z * (int)((l.y >> 16) & 0xff);
      sum += f1.w * (int)((l.y >> 24) & 0xff);
      intermediate_buffer[local_ofs + i] = sum >> WarpHorizBits;
    }
  } else if (active) {
    scale = cb_scale[ref + 1];
    int mv = block.z;
    int mvx = scale_value((mbx << SUBPEL_BITS) + (mv >> 16), scale.x) + SCALE_EXTRA_OFF;
    mvy = scale_value((mby << SUBPEL_BITS) + ((mv << 16) >> 16), scale.z) + SCALE_EXTRA_OFF;
    mvx += (dx + wi) * scale.y;
    mvy += dy * scale.w;
    int x0 = clamp((mvx >> SCALE_SUBPEL_BITS) - 3, -11, ref_w);
    int y0 = (mvy >> SCALE_SUBPEL_BITS) - 3;
    mvx &= SCALE_SUBPEL_MASK;
    mvy &= SCALE_SUBPEL_MASK;
    int filter_h = (((block.y >> 5) & 15) << 4) + (mvx >> SCALE_EXTRA_BITS);
    int lines = 8 + ((3 * scale.w + mvy) >> SCALE_SUBPEL_BITS);
    int4 kernel_h0 = cb_kernels[filter_h][0];
    int4 kernel_h1 = cb_kernels[filter_h][1];

    for (i = 0; i < lines; ++i) {
      int ref_addr = ref_offset + ref_stride * clamp(y0 + i, 0, ref_h) + x0;
      uint3 l = dst_frame.Load3(ref_addr & (~3));
      const uint shift = (ref_addr & 3) * 8;
      l.x = (l.x >> shift) | ((l.y << (24 - shift)) << 8);
      l.y = (l.y >> shift) | ((l.z << (24 - shift)) << 8);
      int sum = 0;
      sum += kernel_h0.x * (int)((l.x >> 0) & 0xff);
      sum += kernel_h0.y * (int)((l.x >> 8) & 0xff);
      sum += kernel_h0.z * (int)((l.x >> 16) & 0xff);
      sum += kernel_h0.w * (int)((l.x >> 24) & 0xff);
      sum += kernel_h1.x * (int)((l.y >> 0) & 0xff);
      sum += kernel_h1.y * (int)((l.y >> 8) & 0xff);
      sum += kernel_h1.z * (int)((l.y >> 16) & 0xff);
      sum += kernel_h1.w * (int)((l.y >> 24) & 0xff);
      intermediate_buffer[local_ofs + i] = (sum + FilterLineAdd8bit) >> FilterLineShift;
    }
  }
  GroupMemoryBarrierWithGroupSync();
  if (active && info0.x) {
    int sy = sy4 + wi * info2.z;
    for (i = 0; i < 4; ++i) {
      int filter_addr =
          WarpFilterSize * (((sy + i * info2.w + WarpFiltRoundAdd) >> WarpFiltRoundBits) + WarpFiltOffset);
      int4 filter0 = warp_filter.Load4(filter_addr);
      int4 filter1 = warp_filter.Load4(filter_addr + 16);
      int local_grp = ((thread.x & 60) + i) * LocalStride + wi;
      output0[i] = intermediate_buffer[local_grp + 0] * filter0.x + intermediate_buffer[local_grp + 1] * filter0.y +
                   intermediate_buffer[local_grp + 2] * filter0.z + intermediate_buffer[local_grp + 3] * filter0.w +
                   intermediate_buffer[local_grp + 4] * filter1.x + intermediate_buffer[local_grp + 5] * filter1.y +
                   intermediate_buffer[local_grp + 6] * filter1.z + intermediate_buffer[local_grp + 7] * filter1.w;
    }
  } else if (active) {
    mvy += wi * scale.w;
    const int filter_v = (((block.y >> 9) & 15) << 4) + ((mvy & SCALE_SUBPEL_MASK) >> SCALE_EXTRA_BITS);
    const int4 kernel_v0 = cb_kernels[filter_v][0];
    const int4 kernel_v1 = cb_kernels[filter_v][1];
    const int local_base = (mvy >> SCALE_SUBPEL_BITS) + (thread.x & 60) * LocalStride;
    for (i = 0; i < 4; ++i) {
      int sum = 0;
      int loc_addr = local_base + i * LocalStride;
      sum += kernel_v0.x * intermediate_buffer[loc_addr + 0];
      sum += kernel_v0.y * intermediate_buffer[loc_addr + 1];
      sum += kernel_v0.z * intermediate_buffer[loc_addr + 2];
      sum += kernel_v0.w * intermediate_buffer[loc_addr + 3];
      sum += kernel_v1.x * intermediate_buffer[loc_addr + 4];
      sum += kernel_v1.y * intermediate_buffer[loc_addr + 5];
      sum += kernel_v1.z * intermediate_buffer[loc_addr + 6];
      sum += kernel_v1.w * intermediate_buffer[loc_addr + 7];
      output0[i] = sum;
    }
  }
  GroupMemoryBarrierWithGroupSync();

  // REF1
  ref = (block.y >> 14) & 7;
  refplane = ref * 3 + plane;
  ref_offset = cb_refplanes[refplane].y;
  ref_stride = cb_refplanes[refplane].x;
  ref_w = cb_refplanes[refplane].z;
  ref_h = cb_refplanes[refplane].w;
  info0 = cb_gm_warp[ref].info0;

  int output1[4];
  if (active && info0.x) {
    int4 info1 = cb_gm_warp[ref].info1;
    info2 = cb_gm_warp[ref].info2;
    const int src_x = ((x & (~7)) + 4) << subsampling;
    const int src_y = ((y & (~7)) + 4) << subsampling;
    const int dst_x = info1.x * src_x + info1.y * src_y + (int)info0.z;
    const int dst_y = info1.z * src_x + info1.w * src_y + (int)info0.w;
    const int x4 = dst_x >> subsampling;
    const int y4 = dst_y >> subsampling;

    int ix4 = clamp((x4 >> WarpPrecBits) - 7 + (x & 7), -11, ref_w);
    int iy4 = (y4 >> WarpPrecBits) - 7 + (y & 7);

    int sx4 = x4 & ((1 << WarpPrecBits) - 1);
    sy4 = y4 & ((1 << WarpPrecBits) - 1);

    sx4 += info2.x * (-4) + info2.y * (-4);
    sy4 += info2.w * (-4) + info2.z * (-4);

    sx4 &= ~((1 << WarpReduceBits) - 1);
    sy4 &= ~((1 << WarpReduceBits) - 1);

    sx4 += info2.y * ((y & 7) - 3) + info2.x * (x & 7);
    sy4 += info2.z * (y & 7) + info2.w * (x & 7);

    ref_offset += ix4 + wi;
    for (i = 0; i < 11; ++i) {
      const int offset = ref_offset + ref_stride * clamp(iy4 + i, 0, ref_h);
      uint3 l = dst_frame.Load3(offset & (~3));
      uint shift = (offset & 3) * 8;
      l.x = (l.x >> shift) | ((l.y << (24 - shift)) << 8);
      l.y = (l.y >> shift) | ((l.z << (24 - shift)) << 8);
      int sum = WarpHorizSumAdd;
      const int sx = sx4 + info2.y * i + wi * info2.x;
      int filter_offset = WarpFilterSize * (((sx + WarpFiltRoundAdd) >> WarpFiltRoundBits) + WarpFiltOffset);
      uint4 f0 = warp_filter.Load4(filter_offset);
      uint4 f1 = warp_filter.Load4(filter_offset + 16);
      sum += f0.x * (int)((l.x >> 0) & 0xff);
      sum += f0.y * (int)((l.x >> 8) & 0xff);
      sum += f0.z * (int)((l.x >> 16) & 0xff);
      sum += f0.w * (int)((l.x >> 24) & 0xff);
      sum += f1.x * (int)((l.y >> 0) & 0xff);
      sum += f1.y * (int)((l.y >> 8) & 0xff);
      sum += f1.z * (int)((l.y >> 16) & 0xff);
      sum += f1.w * (int)((l.y >> 24) & 0xff);
      intermediate_buffer[local_ofs + i] = sum >> WarpHorizBits;
    }
  } else if (active) {
    scale = cb_scale[ref + 1];
    int mv = block.w;
    int mvx = scale_value((mbx << SUBPEL_BITS) + (mv >> 16), scale.x) + SCALE_EXTRA_OFF;
    mvy = scale_value((mby << SUBPEL_BITS) + ((mv << 16) >> 16), scale.z) + SCALE_EXTRA_OFF;
    mvx += (dx + wi) * scale.y;
    mvy += dy * scale.w;
    int x0 = clamp((mvx >> SCALE_SUBPEL_BITS) - 3, -11, ref_w);
    int y0 = (mvy >> SCALE_SUBPEL_BITS) - 3;
    mvx &= SCALE_SUBPEL_MASK;
    mvy &= SCALE_SUBPEL_MASK;
    int filter_h = (((block.y >> 5) & 15) << 4) + (mvx >> SCALE_EXTRA_BITS);
    int lines = 8 + ((3 * scale.w + mvy) >> SCALE_SUBPEL_BITS);
    int4 kernel_h0 = cb_kernels[filter_h][0];
    int4 kernel_h1 = cb_kernels[filter_h][1];

    for (i = 0; i < lines; ++i) {
      int ref_addr = ref_offset + ref_stride * clamp(y0 + i, 0, ref_h) + x0;
      uint3 l = dst_frame.Load3(ref_addr & (~3));
      const uint shift = (ref_addr & 3) * 8;
      l.x = (l.x >> shift) | ((l.y << (24 - shift)) << 8);
      l.y = (l.y >> shift) | ((l.z << (24 - shift)) << 8);
      int sum = 0;
      sum += kernel_h0.x * (int)((l.x >> 0) & 0xff);
      sum += kernel_h0.y * (int)((l.x >> 8) & 0xff);
      sum += kernel_h0.z * (int)((l.x >> 16) & 0xff);
      sum += kernel_h0.w * (int)((l.x >> 24) & 0xff);
      sum += kernel_h1.x * (int)((l.y >> 0) & 0xff);
      sum += kernel_h1.y * (int)((l.y >> 8) & 0xff);
      sum += kernel_h1.z * (int)((l.y >> 16) & 0xff);
      sum += kernel_h1.w * (int)((l.y >> 24) & 0xff);
      intermediate_buffer[local_ofs + i] = (sum + FilterLineAdd8bit) >> FilterLineShift;
    }
  }
  GroupMemoryBarrierWithGroupSync();
  if (active && info0.x) {
    int sy = sy4 + wi * info2.z;
    for (i = 0; i < 4; ++i) {
      int filter_addr =
          WarpFilterSize * (((sy + i * info2.w + WarpFiltRoundAdd) >> WarpFiltRoundBits) + WarpFiltOffset);
      int4 filter0 = warp_filter.Load4(filter_addr);
      int4 filter1 = warp_filter.Load4(filter_addr + 16);
      int local_grp = ((thread.x & 60) + i) * LocalStride + wi;
      output1[i] = intermediate_buffer[local_grp + 0] * filter0.x + intermediate_buffer[local_grp + 1] * filter0.y +
                   intermediate_buffer[local_grp + 2] * filter0.z + intermediate_buffer[local_grp + 3] * filter0.w +
                   intermediate_buffer[local_grp + 4] * filter1.x + intermediate_buffer[local_grp + 5] * filter1.y +
                   intermediate_buffer[local_grp + 6] * filter1.z + intermediate_buffer[local_grp + 7] * filter1.w;
    }
  } else if (active) {
    mvy += wi * scale.w;
    const int filter_v = (((block.y >> 9) & 15) << 4) + ((mvy & SCALE_SUBPEL_MASK) >> SCALE_EXTRA_BITS);
    const int4 kernel_v0 = cb_kernels[filter_v][0];
    const int4 kernel_v1 = cb_kernels[filter_v][1];
    const int local_base = (mvy >> SCALE_SUBPEL_BITS) + (thread.x & 60) * LocalStride;
    for (i = 0; i < 4; ++i) {
      int sum = 0;
      int loc_addr = local_base + i * LocalStride;
      sum += kernel_v0.x * intermediate_buffer[loc_addr + 0];
      sum += kernel_v0.y * intermediate_buffer[loc_addr + 1];
      sum += kernel_v0.z * intermediate_buffer[loc_addr + 2];
      sum += kernel_v0.w * intermediate_buffer[loc_addr + 3];
      sum += kernel_v1.x * intermediate_buffer[loc_addr + 4];
      sum += kernel_v1.y * intermediate_buffer[loc_addr + 5];
      sum += kernel_v1.z * intermediate_buffer[loc_addr + 6];
      sum += kernel_v1.w * intermediate_buffer[loc_addr + 7];
      output1[i] = sum;
    }
  }

  output0[0] = (output0[0] + VertSumAdd) >> VertBits;
  output0[1] = (output0[1] + VertSumAdd) >> VertBits;
  output0[2] = (output0[2] + VertSumAdd) >> VertBits;
  output0[3] = (output0[3] + VertSumAdd) >> VertBits;
  output1[0] = (output1[0] + VertSumAdd) >> VertBits;
  output1[1] = (output1[1] + VertSumAdd) >> VertBits;
  output1[2] = (output1[2] + VertSumAdd) >> VertBits;
  output1[3] = (output1[3] + VertSumAdd) >> VertBits;

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
    coefs.x =
        clamp(DiffWTDBase + ((abs(output0[0] - output1[0]) + DiffWTDRoundAdd) >> DiffWTDRoundShft), 0, DiffWTDMax);
    coefs.y =
        clamp(DiffWTDBase + ((abs(output0[1] - output1[1]) + DiffWTDRoundAdd) >> DiffWTDRoundShft), 0, DiffWTDMax);
    coefs.z =
        clamp(DiffWTDBase + ((abs(output0[2] - output1[2]) + DiffWTDRoundAdd) >> DiffWTDRoundShft), 0, DiffWTDMax);
    coefs.w =
        clamp(DiffWTDBase + ((abs(output0[3] - output1[3]) + DiffWTDRoundAdd) >> DiffWTDRoundShft), 0, DiffWTDMax);
    if ((block.y >> 17) & 1) coefs = int4(64, 64, 64, 64) - coefs;
  }

  output0[0] = blend(output0[0], output1[0], coefs.x);
  output0[1] = blend(output0[1], output1[1], coefs.y);
  output0[2] = blend(output0[2], output1[2], coefs.z);
  output0[3] = blend(output0[3], output1[3], coefs.w);

  if (noskip) {
    const int res_stride = cb_planes[plane].z;
    const int res_offset = cb_planes[plane].w;
    int2 r = (int2)residuals.Load2(res_offset + (x << 1) + y * res_stride);
    output0[0] = clamp(output0[0] + ((r.x << 16) >> 16), 0, 255);
    output0[1] = clamp(output0[1] + (r.x >> 16), 0, 255);
    output0[2] = clamp(output0[2] + ((r.y << 16) >> 16), 0, 255);
    output0[3] = clamp(output0[3] + (r.y >> 16), 0, 255);
  }
  if (active) dst_frame.Store(output_offset, output0[0] | (output0[1] << 8) | (output0[2] << 16) | (output0[3] << 24));

  // The mask of a difference weighted block goes to its chroma planes at half the size. The four threads of
  // a 4x4 put the sums of their lines into the group memory, and one thread of every other 4x4 adds up its
  // own and its neighbour's: a group sync before the memory is used again, and one before the sums are
  // read. (Upstream had no barrier at all here.)
  GroupMemoryBarrierWithGroupSync();
  if (active && compound_type == 2) {
    wi = (thread.x & 63) << 1;
    coefs.x = coefs.x + coefs.y;
    coefs.y = coefs.z + coefs.w;
    intermediate_buffer[wi] = coefs.x;
    intermediate_buffer[wi + 1] = coefs.y;
  }
  GroupMemoryBarrierWithGroupSync();
  if (active && compound_type == 2) {
    if ((wi & 10) == 0)  // filter odd cols and lines
    {
      // next row
      coefs.x = (coefs.x + intermediate_buffer[wi + 2] + 2) >> 2;
      coefs.y = (coefs.y + intermediate_buffer[wi + 3] + 2) >> 2;
      // next 4x4
      coefs.z = (intermediate_buffer[wi + 8] + intermediate_buffer[wi + 10] + 2) >> 2;
      coefs.w = (intermediate_buffer[wi + 9] + intermediate_buffer[wi + 11] + 2) >> 2;

      int chroma_offset = (x + y * cb_planes[1].x) >> 1;
      uint mask = coefs.x | (coefs.y << 8) | (coefs.z << 16) | (coefs.w << 24);
      dst_frame.Store(cb_planes[1].y + chroma_offset, mask);
      dst_frame.Store(cb_planes[2].y + chroma_offset, mask);
    }
  }
}
