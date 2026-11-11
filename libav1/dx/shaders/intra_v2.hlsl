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

// Intra prediction, second version (test player flag -intrav2). Same output
// as intra_main.hlsl + intra_filter.hlsl, different schedule.
//
// The originals run one dispatch pair and one UAV barrier per wavefront
// iteration of the whole frame. Here one thread group owns one 64x64 luma
// region (and its two 32x32 chroma regions), one thread owns one 4x4 cell,
// and the wavefront inside a region runs as passes of a loop inside the
// group: the dependencies between blocks of a region are resolved by group
// syncs, so the host only needs a new dispatch (and a UAV barrier) when a
// block depends on another region. Groups never wait for other groups.
//
// The host (libav1/dx/intra_v2_sched.h) gives every block a step (dispatch)
// and a pass, and stores them per 4x4 cell in `sched`. Blocks are still the
// records written by gen_pred_blocks.hlsl.
//
// Edges are not staged through group memory: each pixel evaluates the edge
// samples it needs as a function of the frame (raw edge, corner filter,
// 5-tap edge filter, 2x upsampling), which leaves two syncs per pass.
//
// intra_v2_hbd.hlsl is this file with HBD defined.

cbuffer IntraDataCommon : register(b0) {
  int4 cb_planes[3];
  int4 cb_flags;
  int4 cb_filter[5][8][2];
  int4 cb_mode_params_lut[16][7];
  int4 cb_sm_weight_arrays[128];
};

cbuffer IntraV2SRT : register(b1) {
  uint cb_list_offset;    // byte offset in `sched` of this step's first region entry
  uint cb_step;           // step handled by this dispatch
  uint cb_passes;         // passes of the busiest region of this step
  uint cb_stamp;          // stamp of this frame's cell entries
  uint cb_map_stride;     // luma cell map stride, in cells
  uint cb_map_uv_offset;  // index of the first chroma cell
  uint cb_reserved0;
  uint cb_reserved1;
};

ByteAddressBuffer pred_blocks : register(t0);
ByteAddressBuffer residuals : register(t1);
ByteAddressBuffer wedge_mask : register(t2);
ByteAddressBuffer sched : register(t3);      // cell map and region list
ByteAddressBuffer block_map : register(t4);  // first record of each block type, per tile
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

#define FilterBucket 9

#ifdef HBD
#define PIXEL_MAX 1023
#define PIXEL_BASE 512
#define PX_SHIFT 1
#else
#define PIXEL_MAX 255
#define PIXEL_BASE 128
#define PX_SHIFT 0
#endif

// 256 luma cells (16x16), then 64 U cells and 64 V cells (8x8 each).
#define REGION_THREADS 384

groupshared int gs_sum_above[REGION_THREADS];      // DC: sum of the 4 above samples of a top-row cell
groupshared int gs_sum_left[REGION_THREADS];       // DC: sum of the 4 left samples of a left-column cell
groupshared int gs_sum_luma[REGION_THREADS];       // CFL: sum of the cell's 16 subsampled luma values
groupshared int gs_fi_bottom[REGION_THREADS * 4];  // filter intra: predicted bottom row of a cell
groupshared int gs_fi_right[REGION_THREADS * 4];   // filter intra: predicted right column of a cell

struct Blk {
  int base;    // address of the block's top-left pixel
  int stride;
  int aa;      // available above pixels
  int la;      // available left pixels
  int bw;
  int bh;
  int flags;   // cb_mode_params_lut[..].z
  int need_al;
  int tl;      // top-left sample
  int filt_a;  // edge filter strength
  int filt_l;
  int fc_a;    // number of filtered samples
  int fc_l;
  int last_a;  // last index a filter tap may reach
  int last_l;
  int up_a;    // edge upsampling
  int up_l;
  int cnt_a;   // edge length before upsampling
  int cnt_l;
};

// Pixel (x, y) relative to the block's top-left pixel; x and y may be -1.
int load_px(int base, int stride, int x, int y) {
#ifdef HBD
  const uint word = dst_frame.Load(base + (x >> 1) * 4 + y * stride);
  return (word >> ((x & 1) << 4)) & 0xffff;
#else
  const uint word = dst_frame.Load(base + (x & ~3) + y * stride);
  return (word >> ((x & 3) << 3)) & 255;
#endif
}

// Edge sample k >= 0 as the originals first load it: the row above (left = 0)
// or the column to the left (left = 1), the last available pixel repeated,
// the other edge or a constant when the edge does not exist.
int raw_edge(Blk b, int left, int k) {
  int x = b.aa ? min(k, b.aa - 1) : -1;
  int y = b.aa ? -1 : 0;
  int value = left ? (PIXEL_BASE + 1) : (PIXEL_BASE - 1);
  if (left) {
    x = b.la ? -1 : 0;
    y = b.la ? min(k, b.la - 1) : -1;
  }
  if (b.aa | b.la) {
    value = load_px(b.base, b.stride, x, y);
    if (!(left && b.la)) value &= PIXEL_MAX;
  }
  return value;
}

int top_left(Blk b) {
  const int t = raw_edge(b, 0, 0);
  const int l = raw_edge(b, 1, 0);
  int tl = (b.la && b.aa) ? load_px(b.base, b.stride, -1, -1) : b.la ? l : b.aa ? t : PIXEL_BASE;
  if ((b.flags & FilterAboveLeftFlag) && (b.bw + b.bh >= 24) && cb_flags.x) tl = ((l + t) * 5 + tl * 6 + 8) >> 4;
  return tl;
}

// Edge sample k >= -1 after the edge filter. Kernels by strength:
// { 0, 4, 8, 4, 0 }, { 0, 5, 6, 5, 0 }, { 2, 4, 4, 4, 2 }.
int filt_edge(Blk b, int left, int k) {
  const int strength = left ? b.filt_l : b.filt_a;
  const int count = left ? b.fc_l : b.fc_a;
  const int last = left ? b.last_l : b.last_a;
  int result = b.tl;
  if (k >= 0) {
    if (strength == 0 || k >= count) {
      result = raw_edge(b, left, k);
    } else {
      const int ef = strength << 2;
      int sum = 8;
      [loop] for (int j = 0; j < 5; ++j) {
        const int weight = ((j == 2 ? 0x4680 : (j == 1 || j == 3) ? 0x4540 : 0x2000) >> ef) & 15;
        int index = (j == 0) ? max(k - 1, 0) : (j < 3) ? (k + j - 1) : min(k + j - 1, last);
        index -= b.need_al;
        if (weight) sum += weight * (index < 0 ? b.tl : raw_edge(b, left, index));
      }
      result = sum >> 4;
    }
  }
  return result;
}

// Edge sample i >= -2 as the prediction reads it, after the optional 2x
// upsampling: even samples are edge samples, odd ones a 4-tap interpolation.
int final_edge(Blk b, int left, int i) {
  const int up = left ? b.up_l : b.up_a;
  const int count = left ? b.cnt_l : b.cnt_a;
  const int interpolate = up && (i & 1) && i > 0 - 2;
  const int k = up ? ((i + 1) >> 1) : i;
  int result = 0;
  if (interpolate) {
    int sum = 8;
    [loop] for (int j = 0; j < 4; ++j) {
      const int index = (j == 3) ? min(k + 1, count - 1) : (k + j - 2);
      const int edge_px = filt_edge(b, left, max(index, -1));
      sum += (j == 1 || j == 2) ? 9 * edge_px : -edge_px;
    }
    result = clamp(sum >> 4, 0, PIXEL_MAX);
  } else {
    result = filt_edge(b, left, max(up ? (i >> 1) : i, -1));
  }
  return result;
}

int compute_cfl_pixel(int dc, int value) {
  value = (value < 0) ? -((-value + (1 << 5)) >> 6) : ((value + (1 << 5)) >> 6);
  return clamp(dc + value, 0, PIXEL_MAX);
}

int intra_inter_blend(int mask, int intra, int inter) {
  return clamp((intra * mask + inter * (64 - mask) + 32) >> 6, 0, PIXEL_MAX);
}

// The 4 subsampled luma values (times 2) under chroma pixels x0..x0+3 of row y.
int4 cfl_luma_row(uint limits, int bx, int by, int x0, int y) {
  const int max_y = (int)(limits >> 16) - 2;
  const int y_stride = cb_planes[0].x;
  const int luma_y = min((by + y) << 1, max_y);
  int4 luma;
#ifdef HBD
  const int max_x = (int)((limits & 0xffff) << 1) - 8;
  const int luma_x = (bx + x0) << 2;
  const int y_offset = cb_planes[0].y + min(luma_x, max_x) + luma_y * y_stride;
  uint4 luma0 = dst_frame.Load4(y_offset);
  uint4 luma1 = dst_frame.Load4(y_offset + y_stride);
  if (luma_x >= max_x) {
    luma0.z = luma0.y;
    luma0.w = luma0.y;
    luma1.z = luma1.y;
    luma1.w = luma1.y;
    if (luma_x > max_x) {
      luma0.x = luma0.y;
      luma1.x = luma1.y;
    }
  }
  luma.x = ((luma0.x >> 0) & 1023) + ((luma0.x >> 16) & 1023) + ((luma1.x >> 0) & 1023) + ((luma1.x >> 16) & 1023);
  luma.y = ((luma0.y >> 0) & 1023) + ((luma0.y >> 16) & 1023) + ((luma1.y >> 0) & 1023) + ((luma1.y >> 16) & 1023);
  luma.z = ((luma0.z >> 0) & 1023) + ((luma0.z >> 16) & 1023) + ((luma1.z >> 0) & 1023) + ((luma1.z >> 16) & 1023);
  luma.w = ((luma0.w >> 0) & 1023) + ((luma0.w >> 16) & 1023) + ((luma1.w >> 0) & 1023) + ((luma1.w >> 16) & 1023);
#else
  const int max_x = (int)(limits & 0xffff) - 4;
  const int luma_x = (bx + x0) << 1;
  const int y_offset = cb_planes[0].y + min(luma_x, max_x) + luma_y * y_stride;
  uint2 luma0 = dst_frame.Load2(y_offset);
  uint2 luma1 = dst_frame.Load2(y_offset + y_stride);
  if (luma_x >= max_x) {
    luma0.y = (luma0.x >> 16) | (luma0.x & 0xffff0000);
    luma1.y = (luma1.x >> 16) | (luma1.x & 0xffff0000);
    if (luma_x > max_x) {
      luma0.x = luma0.y;
      luma1.x = luma1.y;
    }
  }
  luma.x = ((luma0.x >> 0) & 255) + ((luma0.x >> 8) & 255) + ((luma1.x >> 0) & 255) + ((luma1.x >> 8) & 255);
  luma.y = ((luma0.x >> 16) & 255) + ((luma0.x >> 24) & 255) + ((luma1.x >> 16) & 255) + ((luma1.x >> 24) & 255);
  luma.z = ((luma0.y >> 0) & 255) + ((luma0.y >> 8) & 255) + ((luma1.y >> 0) & 255) + ((luma1.y >> 8) & 255);
  luma.w = ((luma0.y >> 16) & 255) + ((luma0.y >> 24) & 255) + ((luma1.y >> 16) & 255) + ((luma1.y >> 24) & 255);
#endif
  luma.x <<= 1;
  luma.y <<= 1;
  luma.z <<= 1;
  luma.w <<= 1;
  return luma;
}

// One row of the 4x2 filter intra unit: taps row .. row + 3 (0 = top row,
// 4 = bottom row) on the corner p0, the 4 pixels above and 2 pixels left.
int4 filter_intra_row(int mode, int row, int p0, int4 above, int p5, int p6) {
  int4 result;
  int4 f0 = cb_filter[mode][row][0];
  int4 f1 = cb_filter[mode][row][1];
  result.x = clamp((p0 * f0.x + above.x * f0.y + above.y * f0.z + above.z * f0.w + above.w * f1.x + p5 * f1.y +
                    p6 * f1.z + 8) >> 4, 0, PIXEL_MAX);
  f0 = cb_filter[mode][row + 1][0];
  f1 = cb_filter[mode][row + 1][1];
  result.y = clamp((p0 * f0.x + above.x * f0.y + above.y * f0.z + above.z * f0.w + above.w * f1.x + p5 * f1.y +
                    p6 * f1.z + 8) >> 4, 0, PIXEL_MAX);
  f0 = cb_filter[mode][row + 2][0];
  f1 = cb_filter[mode][row + 2][1];
  result.z = clamp((p0 * f0.x + above.x * f0.y + above.y * f0.z + above.z * f0.w + above.w * f1.x + p5 * f1.y +
                    p6 * f1.z + 8) >> 4, 0, PIXEL_MAX);
  f0 = cb_filter[mode][row + 3][0];
  f1 = cb_filter[mode][row + 3][1];
  result.w = clamp((p0 * f0.x + above.x * f0.y + above.y * f0.z + above.z * f0.w + above.w * f1.x + p5 * f1.y +
                    p6 * f1.z + 8) >> 4, 0, PIXEL_MAX);
  return result;
}

// Blends with the inter prediction, adds the residual and stores the 4
// pixels of one row. addr is the row's address, (px, py) its plane position.
void finish_row(uint3 block, int plane, int addr, int px, int py, int4 value, int wedge_addr) {
  if (block.y & 0x80000000) {
    const uint wedge = wedge_mask.Load(wedge_addr);
#ifdef HBD
    const uint2 inter = dst_frame.Load2(addr);
    value.x = intra_inter_blend((wedge >> 0) & 255, value.x, (inter.x >> 0) & 1023);
    value.y = intra_inter_blend((wedge >> 8) & 255, value.y, (inter.x >> 16) & 1023);
    value.z = intra_inter_blend((wedge >> 16) & 255, value.z, (inter.y >> 0) & 1023);
    value.w = intra_inter_blend((wedge >> 24) & 255, value.w, (inter.y >> 16) & 1023);
#else
    const uint inter = dst_frame.Load(addr);
    value.x = intra_inter_blend((wedge >> 0) & 255, value.x, (inter >> 0) & 255);
    value.y = intra_inter_blend((wedge >> 8) & 255, value.y, (inter >> 8) & 255);
    value.z = intra_inter_blend((wedge >> 16) & 255, value.z, (inter >> 16) & 255);
    value.w = intra_inter_blend((wedge >> 24) & 255, value.w, (inter >> 24) & 255);
#endif
  }
  if (block.y & (1 << 5)) {
    const uint2 res = residuals.Load2(cb_planes[plane].w + 2 * px + py * cb_planes[plane].z);
    const int res0 = res.x;
    const int res1 = res.y;
    value.x = clamp(value.x + ((res0 << 16) >> 16), 0, PIXEL_MAX);
    value.y = clamp(value.y + (res0 >> 16), 0, PIXEL_MAX);
    value.z = clamp(value.z + ((res1 << 16) >> 16), 0, PIXEL_MAX);
    value.w = clamp(value.w + (res1 >> 16), 0, PIXEL_MAX);
  }
#ifdef HBD
  dst_frame.Store2(addr, uint2(value.x | (value.y << 16), value.z | (value.w << 16)));
#else
  dst_frame.Store(addr, value.x | (value.y << 8) | (value.z << 16) | (value.w << 24));
#endif
}

[numthreads(REGION_THREADS, 1, 1)] void main(uint3 gid : SV_GroupID, uint3 tid : SV_GroupThreadID) {
  const int t = tid.x;
  // Region entry: x = first intra block type of the tile in block_map (24
  // bits) | region column << 24, y = region row | last pass << 8 | step << 16.
  const uint2 entry = sched.Load2(cb_list_offset + gid.x * 8);
  const int is_uv = t >= 256;
  const int cs = is_uv ? 3 : 4;  // log2 of the cells per region row
  const int tc = is_uv ? ((t - 256) & 63) : t;
  const int plane = is_uv ? (1 + ((t - 256) >> 6)) : 0;
  const int cx = ((entry.x >> 24) << cs) + (tc & ((1 << cs) - 1));
  const int cy = ((entry.y & 255) << cs) + (tc >> cs);
  const uint cell_index = is_uv ? (cb_map_uv_offset + cx + cy * (cb_map_stride >> 1)) : (cx + cy * cb_map_stride);
  // Cell entry: x = block index in its type (24 bits) | type << 24,
  // y = step | last pass of the block << 16 | stamp << 24.
  const uint2 cell = sched.Load2(cell_index << 3);
  const int mine = ((cell.y >> 24) == cb_stamp) && ((cell.y & 0xffff) == cb_step);
  const int bucket = mine ? ((cell.x >> 24) & 15) : 8;
  const int is_filter = bucket == FilterBucket;

  uint3 block = uint3(0, 0, 0);
  if (mine) {
    const uint record = block_map.Load(((entry.x & 0xffffff) + bucket) << 2) + (cell.x & 0xffffff) + (plane >> 1);
    block = pred_blocks.Load3(record << 4);
  }

  // block.x: column | row << 16, in 4-pixel units.
  // block.y: see intra_main.hlsl. block.z: mode data.
  const int bw_log = block.y & 7;
  const int bh_log = is_filter ? (block.z & 3) : (8 - bucket - bw_log);
  const int ox = cx - (int)(block.x & 0xffff);  // this cell inside its block
  const int oy = cy - (int)(block.x >> 16);
  const int x0 = ox << 2;
  const int y0 = oy << 2;
  const int bx = (block.x & 0xffff) << 2;
  const int by = (block.x >> 16) << 2;
  const int aa_cells = (block.y >> 10) & 63;
  const int mode = (block.y >> 6) & 15;
  const int4 params = cb_mode_params_lut[mode][(block.y >> 28) & 7];
  const int need_above = (params.z >> NeedAboveShift) & 1;
  const int need_right = (params.z >> NeedRightShift) & 1;
  const int need_left = (params.z >> NeedLeftShift) & 1;
  const int need_bot = (params.z >> NeedBotShift) & 1;
  const int dir_mode = mode < MODE_DC;

  Blk b;
  b.stride = cb_planes[plane].x;
  b.base = cb_planes[plane].y + (bx << PX_SHIFT) + by * b.stride;
  b.aa = aa_cells << 2;
  b.la = ((block.y >> 16) & 63) << 2;
  b.bw = 4 << bw_log;
  b.bh = 4 << bh_log;
  b.flags = params.z;
  b.need_al = (params.z >> NeedAboveLeftShift) & 1;
  b.tl = 0;
  b.cnt_a = need_above * b.bw + need_right * b.bh;
  b.cnt_l = need_left * b.bh + need_bot * b.bw;
  b.up_a = (block.y >> 22) & dir_mode;
  b.up_l = (block.y >> 23) & dir_mode;
  b.filt_a = dir_mode ? ((block.y >> 24) & 3) : 0;
  b.filt_l = dir_mode ? ((block.y >> 26) & 3) : 0;
  b.fc_a = 0;
  if (aa_cells && need_above) b.fc_a = min(aa_cells << (2 + PX_SHIFT), b.bw) + need_right * b.bh + b.need_al - 1;
  b.fc_l = b.fc_a;
  if (b.la && need_left) b.fc_l = min(b.la, b.bh) + need_bot * b.bw + b.need_al - 1;
#ifdef HBD
  b.last_a = b.fc_a;
#else
  b.last_a = b.cnt_a;
#endif
  b.last_l = b.fc_l;

  // A filter intra block takes one pass per anti-diagonal of its cells, the
  // last one for its bottom-right cell.
  const int my_pass = (int)((cell.y >> 16) & 255) - (is_filter ? ((1 << bw_log) - 1 - ox) + ((1 << bh_log) - 1 - oy) : 0);
  const int origin = t - ox - (oy << cs);  // thread of the block's first cell
  const int row_addr = b.base + (x0 << PX_SHIFT) + y0 * b.stride;

  int4 luma0 = int4(0, 0, 0, 0);
  int4 luma1 = int4(0, 0, 0, 0);
  int4 luma2 = int4(0, 0, 0, 0);
  int4 luma3 = int4(0, 0, 0, 0);

  [loop] for (uint pass_i = 0; pass_i < cb_passes; ++pass_i) {
    const int cell_active = mine && (my_pass == (int)pass_i);

    // Phase 1: the sums DC and CFL need from the whole block.
    if (cell_active && !is_filter && mode >= MODE_DC) {
      if (oy == 0 && b.aa) {
        gs_sum_above[t] =
            raw_edge(b, 0, x0) + raw_edge(b, 0, x0 + 1) + raw_edge(b, 0, x0 + 2) + raw_edge(b, 0, x0 + 3);
      }
      if (ox == 0 && b.la) {
        gs_sum_left[t] = raw_edge(b, 1, y0) + raw_edge(b, 1, y0 + 1) + raw_edge(b, 1, y0 + 2) + raw_edge(b, 1, y0 + 3);
      }
      if (mode == MODE_CFL) {
        luma0 = cfl_luma_row(block.z, bx, by, x0, y0);
        luma1 = cfl_luma_row(block.z, bx, by, x0, y0 + 1);
        luma2 = cfl_luma_row(block.z, bx, by, x0, y0 + 2);
        luma3 = cfl_luma_row(block.z, bx, by, x0, y0 + 3);
        gs_sum_luma[t] = luma0.x + luma0.y + luma0.z + luma0.w + luma1.x + luma1.y + luma1.z + luma1.w + luma2.x +
                         luma2.y + luma2.z + luma2.w + luma3.x + luma3.y + luma3.z + luma3.w;
      }
    }

    GroupMemoryBarrierWithGroupSync();

    // Phase 2: predict and store this cell.
    if (cell_active && is_filter) {
      const int fmode = (block.z >> 4) & 7;
      int4 above;
      int4 lf;
      int p0;
      if (oy == 0) {
        above = int4(raw_edge(b, 0, x0), raw_edge(b, 0, x0 + 1), raw_edge(b, 0, x0 + 2), raw_edge(b, 0, x0 + 3));
        if (ox)
          p0 = raw_edge(b, 0, x0 - 1);
        else
          p0 = (b.la && b.aa) ? load_px(b.base, b.stride, -1, -1) : (b.la || b.aa) ? raw_edge(b, 0, 0) : PIXEL_BASE;
      } else {
        const int up = (t - (1 << cs)) * 4;
        above = int4(gs_fi_bottom[up], gs_fi_bottom[up + 1], gs_fi_bottom[up + 2], gs_fi_bottom[up + 3]);
        if (ox)
          p0 = gs_fi_bottom[up - 1];
        else
          p0 = raw_edge(b, 1, y0 - 1);
      }
      if (ox == 0) {
        lf = int4(raw_edge(b, 1, y0), raw_edge(b, 1, y0 + 1), raw_edge(b, 1, y0 + 2), raw_edge(b, 1, y0 + 3));
      } else {
        const int prev = (t - 1) * 4;
        lf = int4(gs_fi_right[prev], gs_fi_right[prev + 1], gs_fi_right[prev + 2], gs_fi_right[prev + 3]);
      }
      const int4 row0 = filter_intra_row(fmode, 0, p0, above, lf.x, lf.y);
      const int4 row1 = filter_intra_row(fmode, 4, p0, above, lf.x, lf.y);
      const int4 row2 = filter_intra_row(fmode, 0, lf.y, row1, lf.z, lf.w);
      const int4 row3 = filter_intra_row(fmode, 4, lf.y, row1, lf.z, lf.w);
      gs_fi_bottom[t * 4] = row3.x;
      gs_fi_bottom[t * 4 + 1] = row3.y;
      gs_fi_bottom[t * 4 + 2] = row3.z;
      gs_fi_bottom[t * 4 + 3] = row3.w;
      gs_fi_right[t * 4] = row0.w;
      gs_fi_right[t * 4 + 1] = row1.w;
      gs_fi_right[t * 4 + 2] = row2.w;
      gs_fi_right[t * 4 + 3] = row3.w;
      finish_row(block, plane, row_addr, bx + x0, by + y0, row0, 0);
      finish_row(block, plane, row_addr + b.stride, bx + x0, by + y0 + 1, row1, 0);
      finish_row(block, plane, row_addr + 2 * b.stride, bx + x0, by + y0 + 2, row2, 0);
      finish_row(block, plane, row_addr + 3 * b.stride, bx + x0, by + y0 + 3, row3, 0);
    }

    if (cell_active && !is_filter) {
      int dc = 0;
      int avrg = 0;
      int alpha = 0;
      int top_right = 0;
      int bottom_left = 0;
      if (b.need_al) b.tl = top_left(b);
      if (mode >= MODE_DC) {
        int count = 0;
        if (b.aa) {
          [loop] for (int i = 0; i < (b.bw >> 2); ++i) dc += gs_sum_above[origin + i];
          count += b.bw;
        }
        if (b.la) {
          [loop] for (int i = 0; i < (b.bh >> 2); ++i) dc += gs_sum_left[origin + (i << cs)];
          count += b.bh;
        }
        dc += count >> 1;
        dc = count ? (int)((uint)dc / (uint)count) : PIXEL_BASE;
        if (mode == MODE_CFL) {
          const int bsize_log = bw_log + bh_log + 2;
          int total = 0;
          [loop] for (int j = 0; j < (b.bh >> 2); ++j) {
            [loop] for (int i = 0; i < (b.bw >> 2); ++i) total += gs_sum_luma[origin + i + (j << cs)];
          }
          avrg = (total + (2 << bsize_log)) >> (bsize_log + 2);
          alpha = (int)((block.y >> 22) & 63) - 16;
        }
      } else {
        if (b.flags & SmoothH) top_right = final_edge(b, 0, b.bw - 1);
        if (b.flags & SmoothV) bottom_left = final_edge(b, 1, b.bh - 1);
      }
      const int count_a = b.cnt_a << b.up_a;
      const int count_l = b.cnt_l << b.up_l;

      [loop] for (int r = 0; r < 4; ++r) {
        const int y = y0 + r;
        const int4 luma = (r == 0) ? luma0 : (r == 1) ? luma1 : (r == 2) ? luma2 : luma3;
        int4 value = int4(dc, dc, dc, dc);
        if (mode == MODE_CFL) {
          value.x = compute_cfl_pixel(dc, (luma.x - avrg) * alpha);
          value.y = compute_cfl_pixel(dc, (luma.y - avrg) * alpha);
          value.z = compute_cfl_pixel(dc, (luma.z - avrg) * alpha);
          value.w = compute_cfl_pixel(dc, (luma.w - avrg) * alpha);
        }
        if (mode < MODE_INTRA_BC) {
          [loop] for (int i = 0; i < 4; ++i) {
            const int x = x0 + i;
            const int offset_x = (x << 6) + (y + 1) * params.x;
            const int offset_y = (y << 6) + (x + 1) * params.y;
            const int shift_x = ((offset_x << b.up_a) & 0x3F) >> 1;
            const int shift_y = ((offset_y << b.up_l) & 0x3F) >> 1;
            const int base_x = offset_x >> (6 - b.up_a);
            const int base_y = offset_y >> (6 - b.up_l);

            int mode_info = params.z & 15;
            if (mode_info == Dir2) mode_info = (base_x >= 0 - (1 << b.up_a)) ? Dir1 : Dir3;
            const int paeth = mode == MODE_PAETH;

            // Only the samples with a weight are read.
            int t0 = 0;
            int t1 = 0;
            int l0 = 0;
            int l1 = 0;
            if ((mode_info & SmoothV) || mode_info == Dir1 || paeth)
              t0 = final_edge(b, 0, clamp(base_x, -2, count_a - 1));
            if (mode_info == Dir1 && shift_x) t1 = final_edge(b, 0, clamp(base_x + 1, -2, count_a - 1));
            if ((mode_info & SmoothH) || mode_info == Dir3 || paeth)
              l0 = final_edge(b, 1, clamp(base_y, -2, count_l - 1));
            if (mode_info == Dir3 && shift_y) l1 = final_edge(b, 1, clamp(base_y + 1, -2, count_l - 1));

            int w_t0 = (mode_info & SmoothV) ? cb_sm_weight_arrays[b.bh + y].x : (mode_info == Dir1) ? (32 - shift_x) : 0;
            int w_l0 = (mode_info & SmoothH) ? cb_sm_weight_arrays[b.bw + x].x : (mode_info == Dir3) ? (32 - shift_y) : 0;
            if (paeth) {
              int diff_t = b.tl - l0;
              int diff_l = b.tl - t0;
              const int diff_tl = abs(diff_t + diff_l);
              diff_t = abs(diff_t);
              diff_l = abs(diff_l);
              w_l0 = (diff_l <= diff_t && diff_l <= diff_tl) ? 32 : 0;
              w_t0 = (diff_t < diff_l && diff_t <= diff_tl) ? 32 : 0;
            }
            const int w_tl = paeth ? (32 - w_t0 - w_l0) : 0;
            const int w_l1 = (mode_info == Dir3) ? shift_y : 0;
            const int w_t1 = (mode_info == Dir1) ? shift_x : 0;
            const int w_b = (mode_info & SmoothV) ? (0x100 - w_t0) : 0;
            const int w_r = (mode_info & SmoothH) ? (0x100 - w_l0) : 0;

            int sum = w_tl * b.tl + w_b * bottom_left + w_r * top_right + w_l0 * l0 + w_t0 * t0 + w_l1 * l1 + w_t1 * t1;
            sum += 1 << (params.w - 1);
            sum >>= params.w;
            sum = clamp(sum, 0, PIXEL_MAX);
            if (i == 0) value.x = sum;
            if (i == 1) value.y = sum;
            if (i == 2) value.z = sum;
            if (i == 3) value.w = sum;
          }
        }
        finish_row(block, plane, row_addr + r * b.stride, bx + x0, by + y, value, (block.z << 6) + x0 + y * b.bw);
      }
    }

    AllMemoryBarrierWithGroupSync();
  }
}
