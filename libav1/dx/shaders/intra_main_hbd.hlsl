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

#ifdef INTRA_RUN
// intra_run*.hlsl: one thread group runs the wavefront iterations [0, cb_run_iters) of its lists, which start
// at the word cb_run_lists of the block map (prediction.rs). It reads the starts of each iteration's lists
// from the block map instead of the constants of one iteration.
cbuffer IntraRunSRT : register(b1) {
  uint cb_run_lists;
  uint cb_run_iters;
  uint cb_run_reserved0;
  uint cb_run_reserved1;
};
#else
cbuffer PSSLIntraSRT : register(b1) {
  int4 cb_counts0;
  int4 cb_counts1;
  uint cb_wi_count;
  int cb_pass_offset;
};
#endif

ByteAddressBuffer pred_blocks : register(t0);
ByteAddressBuffer residuals : register(t1);
ByteAddressBuffer wedge_mask : register(t2);
#ifdef INTRA_RUN
ByteAddressBuffer block_map : register(t3);
#endif
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
// The threads of a group: WG_SIZE, or with INTRA_RUN that many WG_SIZE slots. A block of 256 lanes and
// more is predicted in pieces of WG_SIZE lanes, each piece with its own edges, whichever group runs it.
#ifdef INTRA_RUN
#define GROUP_THREADS (WG_SIZE * INTRA_RUN_SLOTS)
#else
#define GROUP_THREADS WG_SIZE
#endif
#define EDGE_ABOVE 0
#define EDGE_LEFT 1

// INTRA_PREFETCH (intra_main_single_edge*.hlsl, the default, and intra_main_prefetch*.hlsl): each
// lane loads its edges and the plane's constants before the first sync, the prepared edges go to a
// bank per transform, and V and H prediction without an angle delta copies the edge samples directly.
// INTRA_8BIT (intra_main_prefetch.hlsl, with INTRA_PREFETCH): the same shader for 8-bit frames. It keeps
// the samples it predicts as this one does, two to a word of 16-bit halves, and reads and writes the
// frame's 8-bit samples (four to a word) at the edges of that: the loads of the edges, of the luma of
// CFL, of an intra block copy and of an inter-intra block's inter prediction, and the store.
#ifdef INTRA_8BIT
#define PIXEL_MAX 255
#define PIXEL_MID 128
#define LEFT_SHIFT 24
#else
#define PIXEL_MAX 1023
#define PIXEL_MID 512
#define LEFT_SHIFT 16
#endif

// FXC can turn dynamic masks into bit-field instructions with run-time widths.
static const uint low_bits[11] = {0, 1, 3, 7, 15, 31, 63, 127, 255, 511, 1023};

// Each transform owns 20 words per four lanes on each edge, including room for the corner and
// upsampling. DC and CFL blocks use the same space for their partial sums.
#ifdef INTRA_PREFETCH
groupshared int edges[GROUP_THREADS * 10];
#define EDGE(side, index) edges[index]
#else
groupshared int edges[2][(WG_SIZE / 4) * 20];
#define EDGE(side, index) edges[side][index]
#endif

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
  [loop] for (int i = wi; i < count; i += step) sum += EDGE(edge, base + i);
  if (step > 1) sum = WaveActiveSum(sum);
  return sum;
}
#endif

int filter_edge(int edge, int base, int i, int last, int strength) {
  const int outer = (0x2000 >> (4 * strength)) & 15;
  const int inner = (0x4540 >> (4 * strength)) & 15;
  const int center = (0x4680 >> (4 * strength)) & 15;
  return (8 + outer * (EDGE(edge, base + max(i - 1, 0)) + EDGE(edge, base + min(i + 3, last))) +
          inner * (EDGE(edge, base + i) + EDGE(edge, base + min(i + 2, last))) +
          center * EDGE(edge, base + i + 1)) >> 4;
}

int upsample_edge(int p0, int p1, int p2, int p3) {
  return clamp((-p0 + 9 * (p1 + p2) - p3 + 8) >> 4, 0, PIXEL_MAX);
}

#ifdef INTRA_PREFETCH
// Read the raw edge and publish the prepared samples in a disjoint range.
// Filtering and upsampling are mutually exclusive for a given edge in
// gen_pred_blocks. Upsampling preserves the original even samples.
void prepare_bank(int raw, int prepared, int wi, int wi_count, int count, int upsample,
                  int filter_count, int strength, int need_corner) {
  if (raw == prepared) return;
  if (wi == 0 && need_corner) {
    EDGE(0, prepared - 2) = EDGE(0, raw - 2);
    if (!upsample) EDGE(0, prepared - 1) = EDGE(0, raw - 1);
  }
  if (upsample) {
    if (wi >= (count >> 1)) return;
    const int i = wi * 2;
    const int p0 = EDGE(0, raw + i - 2);
    const int p1 = EDGE(0, raw + i - 1);
    const int p2 = EDGE(0, raw + i);
    const int p3 = EDGE(0, raw + i + 1);
    const int p4 = EDGE(0, raw + min(i + 2, count - 1));
    EDGE(0, prepared + 2 * i - 1) = upsample_edge(p0, p1, p2, p3);
    EDGE(0, prepared + 2 * i) = p2;
    EDGE(0, prepared + 2 * i + 1) = upsample_edge(p1, p2, p3, p4);
    EDGE(0, prepared + 2 * i + 2) = p3;
  } else {
    [loop] for (int i = wi; i < count; i += wi_count) {
      EDGE(0, prepared + i) = i < filter_count
        ? filter_edge(0, raw - need_corner, i, filter_count, strength)
        : EDGE(0, raw + i);
    }
  }
}
#else
// In gen_pred_blocks.hlsl, an edge is upsampled only for a small enough block and an angle
// difference below 40, where its filter strength is zero. Each lane prepares its outputs
// from the raw edge here, then writes them back after every lane has finished reading.
int4 prepare_edge(int edge, int base, int wi, int wi_count, int count, int upsample,
                  int filter_count, int strength, int need_aboveleft) {
  int4 values = 0;
  if (upsample && wi < (count >> 1)) {
    const int i = wi * 2;
    const int p0 = EDGE(edge, base + i - 2);
    const int p1 = EDGE(edge, base + i - 1);
    const int p2 = EDGE(edge, base + i);
    const int p3 = EDGE(edge, base + i + 1);
    const int p4 = EDGE(edge, base + min(i + 2, count - 1));
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
    EDGE(edge, i - 1) = values.x;
    EDGE(edge, i) = values.y;
    EDGE(edge, i + 1) = values.z;
    EDGE(edge, i + 2) = values.w;
  } else if (strength && wi < filter_count) {
    EDGE(edge, base + wi) = values.x;
    if (wi + wi_count < filter_count) EDGE(edge, base + wi + wi_count) = values.y;
  }
}

#endif

#ifdef INTRA_8BIT
// The pair of samples wi of the row above (available: the pairs that are there), as two 16-bit halves.
uint load_above_pair(int corner, int wi, int available) {
  const int at = min(wi, available - 1);
  const uint word = dst_frame.Load(corner + 4 + 4 * (at >> 1));
  const uint half_word = (at & 1) ? word >> 16 : word & 0xffff;
  uint pair = (half_word & 255) | ((half_word >> 8) << 16);
  if (wi >= available) pair = (pair & 0xffff0000) | (pair >> 16);
  return pair;
}

// Four 8-bit samples of a frame word as two words of 16-bit halves, and back.
uint2 unpack_samples(uint word) {
  return uint2((word & 255) | (((word >> 8) & 255) << 16), ((word >> 16) & 255) | ((word >> 24) << 16));
}

uint pack_samples(uint2 pixels) {
  return (pixels.x & 255) | (((pixels.x >> 16) & 255) << 8) | ((pixels.y & 255) << 16) |
         (((pixels.y >> 16) & 255) << 24);
}
#else
uint load_above_pair(int corner, int wi, int available) {
  uint pair = dst_frame.Load(corner + 4 + 4 * min(wi, available - 1));
  if (wi >= available) pair = (pair & 0xffff0000) | (pair >> 16);
  return pair;
}
#endif

int load_left(int corner, int stride, int wi, int available) {
  return dst_frame.Load(corner + (min(wi, available - 1) + 1) * stride) >> LEFT_SHIFT;
}

uint2 predict_straight(bool vertical, int corner, int stride, int x, int y, int above, int left) {
  [branch] if (vertical && above) {
    const int pair = x >> 1;
    return uint2(load_above_pair(corner, pair, above), load_above_pair(corner, pair + 1, above));
  }
  uint sample = vertical ? PIXEL_MID - 1 : PIXEL_MID + 1;
  [branch] if (left) sample = load_left(corner, stride, vertical ? 0 : y, left);
  else [branch] if (above) sample = dst_frame.Load(corner + 4) & PIXEL_MAX;
  sample |= sample << 16;
  return uint2(sample, sample);
}

int compute_bc(int p0, int p1, int p2, int p3, int fh, int fv) {
  int l0 = ((1 << 14) + p0 * fh + p1 * (128 - fh) + (1 << (FilterLineShift - 1))) >> FilterLineShift;
  int l1 = ((1 << 14) + p2 * fh + p3 * (128 - fh) + (1 << (FilterLineShift - 1))) >> FilterLineShift;
  int output = SumAddVert + l0 * fv + l1 * (128 - fv);
  return clamp((output >> OutputShift) - OutputSub, 0, PIXEL_MAX);
}

#ifdef INTRA_PREFETCH
// The flags of cb_mode_params_lut[mode][mode_angle] (its z), which host/tables.rs has: for the
// directional modes by the zone of the angle (the nominal angle of V H D45 D135 D113 D157 D203 D67 and
// 3 degrees a step of mode_angle - 3), for the others by the mode.
int intra_mode_flags(int mode, int mode_angle) {
  if (mode < 8) {
    const uint nominal = ((mode < 4 ? 0x872DB45Au : 0x43CB9D71u) >> (8 * (mode & 3))) & 255;
    const int angle = (int)nominal + 3 * (mode_angle - 3);
    return angle < 90 ? 305 : angle == 90 ? 273 : angle < 180 ? 850 : angle == 180 ? 323 : 451;
  }
  return mode == 8 ? 92 : mode == 9 ? 84 : mode == 10 ? 88 : mode == MODE_PAETH ? 336 : mode == MODE_INTRA_BC ? 0 : 80;
}
#endif

#ifdef INTRA_KIND_SORT
// INTRA_KIND_SORT (intra_main_sorted*.hlsl, with INTRA_PREFETCH and INTRA_SINGLE_EDGE): a thread group first
// sorts its blocks by the path they take through this shader, so that a wave runs one path. A wavefront
// step's lists hold the blocks of every prediction kind in decode order, a 4x4 block is four lanes, and a
// wave of small blocks mixes DC, CFL, smooth and directional blocks and runs all of their branches one after
// the other: on the console a step took about 3.3 us more for every kind it had (21 us with one kind, 43 us
// with seven; docs/test-player.md, "Intra step timing"). The blocks keep their lanes together, each a
// contiguous range in the order of the keys; which block a thread predicts changes, what it predicts does
// not.
#define SORT_KEYS 16
groupshared uint sort_counts[SORT_KEYS + 1];
groupshared uint sort_leads[GROUP_THREADS];
groupshared uint4 sort_items[GROUP_THREADS];

// The path of a block: DC, CFL, smooth, Paeth, zone 1 or 3, zone 2, exact vertical or horizontal, intra block
// copy, with 8 more for the inter-intra blend. Kinds that share code are next to each other.
uint intra_sort_key(uint3 block) {
  const int mode = (block.y >> 6) & 15;
  const int mode_angle = (block.y >> 28) & 7;
  const int info = intra_mode_flags(mode, mode_angle) & 15;
  uint key = 2;
  if (mode == MODE_CFL && ((block.y >> 22) & 63) != 16) key = 1;
  else if (mode >= MODE_DC) key = 0;
  else if (mode == MODE_INTRA_BC) key = 7;
  else if (mode < 2 && mode_angle == 3) key = 6;
  else if (info == Dir2) key = 5;
  else if (info == Dir1 || info == Dir3) key = 4;
  else if (mode == MODE_PAETH) key = 3;
  return key | ((block.y >> 28) & 8);
}
#endif

#if defined(INTRA_PREFETCH) && defined(INTRA_8BIT)
// The four samples at (x, y) of an intra block copy block: the motion vector in block.z.
uint2 predict_intra_bc(uint3 block, int4 plane_cb, int stride, int x, int y) {
  const int mv = (int)block.z;
  const int mvx = x + (mv >> (16 + SubpelBits));
  const int mvy = y + ((mv << 16) >> (16 + SubpelBits));
  const int filt_h = 128 >> ((mv >> 19) & 1);
  const int filt_v = 128 >> ((mv >> 3) & 1);
  int addr = plane_cb.y + mvx + mvy * stride;
  uint2 ref0, ref1 = 0;

  const uint shift = (addr & 3) << 3;
  addr &= ~3;
  ref0 = dst_frame.Load2(addr);
  ref0.x = (ref0.x >> shift) | ((ref0.y << (24 - shift)) << 8);
  ref0.y = ref0.y >> shift;

  if (filt_v != 128) {
    ref1 = dst_frame.Load2(addr + stride);
    ref1.x = (ref1.x >> shift) | ((ref1.y << (24 - shift)) << 8);
    ref1.y = ref1.y >> shift;
  }

  uint2 pixels;
  pixels.x = compute_bc((ref0.x >> 0) & 255, (ref0.x >> 8) & 255, (ref1.x >> 0) & 255, (ref1.x >> 8) & 255, filt_h,
                        filt_v) |
             (compute_bc((ref0.x >> 8) & 255, (ref0.x >> 16) & 255, (ref1.x >> 8) & 255, (ref1.x >> 16) & 255,
                         filt_h, filt_v)
              << 16);
  pixels.y = compute_bc((ref0.x >> 16) & 255, (ref0.x >> 24) & 255, (ref1.x >> 16) & 255, (ref1.x >> 24) & 255,
                        filt_h, filt_v) |
             (compute_bc((ref0.x >> 24) & 255, (ref0.y >> 0) & 255, (ref1.x >> 24) & 255, (ref1.y >> 0) & 255,
                         filt_h, filt_v)
              << 16);
  return pixels;
}
#elif defined(INTRA_PREFETCH)
// The four samples at (x, y) of an intra block copy block: the motion vector in block.z.
uint2 predict_intra_bc(uint3 block, int4 plane_cb, int stride, int x, int y) {
  const int mv = (int)block.z;
  const int mvx = x + (mv >> (16 + SubpelBits));
  const int mvy = y + ((mv << 16) >> (16 + SubpelBits));
  const int filt_h = 128 >> ((mv >> 19) & 1);
  const int filt_v = 128 >> ((mv >> 3) & 1);
  int addr = plane_cb.y + (mvx << 1) + mvy * stride;
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

  uint2 pixels;
  pixels.x = compute_bc((ref0.x >> 0) & 1023, (ref0.x >> 16) & 1023, (ref1.x >> 0) & 1023, (ref1.x >> 16) & 1023,
                        filt_h, filt_v) |
             (compute_bc((ref0.x >> 16) & 1023, (ref0.y >> 0) & 1023, (ref1.x >> 16) & 1023, (ref1.y >> 0) & 1023,
                         filt_h, filt_v)
              << 16);
  pixels.y = compute_bc((ref0.y >> 0) & 1023, (ref0.y >> 16) & 1023, (ref1.y >> 0) & 1023, (ref1.y >> 16) & 1023,
                        filt_h, filt_v) |
             (compute_bc((ref0.y >> 16) & 1023, (ref0.z >> 0) & 1023, (ref1.y >> 16) & 1023, (ref1.z >> 0) & 1023,
                         filt_h, filt_v)
              << 16);
  return pixels;
}
#endif

uint compute_cfl_pixel(int dc, int value) {
  value = (value < 0) ? -((-value + (1 << 5)) >> 6) : ((value + (1 << 5)) >> 6);
  return clamp(dc + value, 0, PIXEL_MAX);
}

int intra_inter_blend(int mask, int intra, int inter) {
  return clamp((intra * mask + inter * (64 - mask) + 32) >> 6, 0, PIXEL_MAX);
}

#ifdef INTRA_8BIT
// The sums of the 2x2 luma samples under four chroma samples, doubled (as the 8-bit intra_main.hlsl
// takes them).
int4 cfl_luma_row(uint limits, int bx, int by, int x0, int y) {
  int4 luma;
  const int max_x = (limits & 0xffff) - 4;
  const int max_y = (limits >> 16) - 2;
  const int y_stride = cb_planes[0].x;
  const int luma_x = (bx + x0) << 1;
  const int luma_y = min((by + y) << 1, max_y);
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
  luma <<= 1;
  return luma;
}
#else
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
#endif

// One wavefront iteration's work for the thread `thread_x` of the dispatch (of the iteration, in the run),
// at `local` in its group: the records of the nine lists by block size that start at `pass_offset`, with
// `counts0`, `counts1` the records of the first eight and `total` lanes in all (the last list takes the rest).
void intra_main_body(uint thread_x, int local, int4 counts0, int4 counts1, uint total, int pass_offset) {
  const uint3 thread = uint3(thread_x, 0, 0);
  int bsize_log = 10;
  int offset = pass_offset;
  int threadx = thread.x;
  uint3 block = uint3(0, 0, 0);
  int block_lane = 0;
  int wi_count = 0;
  int wi = 1024;
  // The thread has a block, and its place in the group.
  bool active = thread.x < total;
  if (active) {
    if (threadx >= (counts0.x << 10)) {
      offset += counts0.x;
      threadx -= counts0.x << 10;
      bsize_log = 9;
      if (threadx >= (counts0.y << 9)) {
        offset += counts0.y;
        threadx -= counts0.y << 9;
        bsize_log = 8;
        if (threadx >= (counts0.z << 8)) {
          offset += counts0.z;
          threadx -= counts0.z << 8;
          bsize_log = 7;
          if (threadx >= (counts0.w << 7)) {
            offset += counts0.w;
            threadx -= counts0.w << 7;
            bsize_log = 6;
            if (threadx >= (counts1.x << 6)) {
              offset += counts1.x;
              threadx -= counts1.x << 6;
              bsize_log = 5;
              if (threadx >= (counts1.y << 5)) {
                offset += counts1.y;
                threadx -= counts1.y << 5;
                bsize_log = 4;
                if (threadx >= (counts1.z << 4)) {
                  offset += counts1.z;
                  threadx -= counts1.z << 4;
                  bsize_log = 3;
                  if (threadx >= (counts1.w << 3)) {
                    offset += counts1.w;
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
#ifdef INTRA_KIND_SORT
  // A counting sort of the group's blocks by key. The first lane of a block (wi 0: a block of 256 lanes
  // or more is a group of its own) takes the block's lanes in its key's range; every lane of the block
  // then puts what it predicts at its place there, and every thread takes what is at its own place.
  // Threads without a block go behind the last block.
  const uint sort_key = active ? intra_sort_key(block) : SORT_KEYS;
  if (local <= SORT_KEYS) sort_counts[local] = 0;
  GroupMemoryBarrierWithGroupSync();
  if (active && wi == 0) {
    uint first;
    InterlockedAdd(sort_counts[sort_key], (uint)wi_count, first);
    sort_leads[local] = first;
  }
  GroupMemoryBarrierWithGroupSync();
  uint sorted = 0;
  uint sort_counts_before = 0;
  [loop] for (uint key = 0; key < SORT_KEYS; ++key) {
    const uint count = sort_counts[key];
    if (key < sort_key) sort_counts_before += count;
    sorted += count;
  }
  if (active) {
    sort_items[sort_counts_before + sort_leads[local - wi] + wi] =
        uint4(block.x, block.y, block.z, (uint)block_lane | ((uint)bsize_log << 16));
  }
  GroupMemoryBarrierWithGroupSync();
  active = (uint)local < sorted;
  block = uint3(0, 0, 0);
  block_lane = 0;
  bsize_log = 10;
  wi_count = 0;
  wi = 1024;
  if (active) {
    const uint4 item = sort_items[local];
    block = item.xyz;
    block_lane = (int)(item.w & 0xffff);
    bsize_log = (int)(item.w >> 16);
    wi_count = min(WG_SIZE, 1 << bsize_log);
    wi = block_lane & (WG_SIZE - 1);
  }
#endif
  const int bw_log = block.y & 7;  // 0..4
  const int bw = 4 << bw_log;
  const int bh = 1 << (bsize_log - bw_log);

  int bx = (block.x & 0xffff) << 2;
  int by = (block.x >> 16) << 2;

  const int plane = (block.y >> 3) & 3;
  const int above_available = ((block.y >> 10) & 63) << 1;
  const int left_available = ((block.y >> 16) & 63) << 2;

  const int mode = (block.y >> 6) & 15;
#ifdef INTRA_PREFETCH
  // A zero CfL alpha adds zero to the DC prediction. The sibling chroma
  // plane keeps its own alpha, and the scheduler still accounts for luma.
  const bool cfl = mode == MODE_CFL && ((block.y >> 22) & 63) != 16;
#else
#define cfl (mode == MODE_CFL)
#endif

  const int mode_angle = (block.y >> 28) & 7;
  int4 params = cb_mode_params_lut[mode][mode_angle];
#ifdef INTRA_PREFETCH
  // What the edges and the loads before the first sync depend on, the flags of the table's entry (z),
  // follow from the mode and the angle; the entry itself (the steps of a directional predictor, the
  // shift of a smooth one) is only read behind the syncs. A lane loads its edges without waiting for
  // a second load behind the record.
  const int mode_flags = intra_mode_flags(mode, mode_angle);
  const bool straight = mode < 2 && mode_angle == 3 && active;
#else
#define mode_flags params.z
  const bool straight = false;
#endif

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

#ifdef INTRA_PREFETCH
  // The plane's constants by a select of the three, which the shader loads at its start, and not by
  // a load that waits for the record.
  const int4 plane_cb = plane == 0 ? cb_planes[0] : plane == 1 ? cb_planes[1] : cb_planes[2];
#else
#define plane_cb cb_planes[plane]
#endif
  const int stride = plane_cb.x;
#ifdef INTRA_8BIT
  const int corner = plane_cb.y + (bx - 4) + (by - 1) * stride;
#else
  const int corner = plane_cb.y + ((bx << 1) - 4) + (by - 1) * stride;
#endif

#ifdef INTRA_PREFETCH
  const int local_start = local - wi;
#else
  const int loc_base = ((local - wi) >> 2) * 20 + 2;
#endif
  const int need_above = (mode_flags >> NeedAboveShift) & 1;
  const int need_right = (mode_flags >> NeedRightShift) & 1;
  const int need_left = (mode_flags >> NeedLeftShift) & 1;
  const int need_bot = (mode_flags >> NeedBotShift) & 1;
  const int need_aboveleft = (mode_flags >> NeedAboveLeftShift) & 1;
  const int above_count = need_above * bw + need_right * bh;
  const int left_count = need_left * bh + need_bot * bw;
  const int y = block_lane >> bw_log;
  const int x0 = (block_lane & low_bits[bw_log]) << 2;

  const int mode_info = mode_flags & 15;
  const bool directional = !straight && mode_info >= Dir1 && mode_info <= Dir3;
  const int upsample_above = directional ? ((block.y >> 22) & 1) : 0;
  const int upsample_left = directional ? ((block.y >> 23) & 1) : 0;
  const int strength_above = directional ? ((block.y >> 24) & 3) : 0;
  const int strength_left = directional ? ((block.y >> 26) & 3) : 0;
  // AV1 7.11.2.4: the edge filter of the row above takes numPx = Min(w, maxX - x + 1) + (pAngle < 90 ?
  // h : 0) + 1 samples, the corner included: those of the row above that are in the frame (above_available
  // counts them in pairs), not more. Samples right of maxX that a larger count filters are read by the
  // predictor of the samples right of maxX, which chroma from luma reads (MaxLumaW runs to the end of the
  // luma transform block, 7.11.5).
  const int filter_above = above_available && need_above
                               ? min(above_available << 1, bw) + need_right * bh + need_aboveleft - 1
                               : 0;
  const int filter_left = left_available && need_left
                              ? min(left_available, bh) + need_bot * bw + need_aboveleft - 1
                              : 0;

#ifdef INTRA_PREFETCH
  // Keep the same 40 words per four lanes: 10 for each raw edge and 20 for
  // preparation, including each edge's two corner slots. For 4x4, a zone 1/3
  // edge can span eight raw or sixteen prepared samples; it takes the entire
  // prepared range. With both edges (zone 2) each fits in half. Larger
  // transforms have proportionally more space, since the range scales with area.
  const int raw_above = local_start * 10 + 2;
  const int raw_stride = (wi_count >> 1) * 5;
  const int raw_left = raw_above + raw_stride;
  const int bank_start = raw_left + raw_stride;
  const bool process_above = above_count && (upsample_above || (strength_above && filter_above));
  const bool process_left = left_count && (upsample_left || (strength_left && filter_left));
  const int prepared_above = process_above ? bank_start : raw_above;
  const int prepared_left = process_left ? bank_start + (process_above ? raw_stride : 0) : raw_left;
#else
#define raw_above loc_base
#define raw_left loc_base
#define edge_base loc_base
#endif

#ifdef INTRA_PREFETCH
  // Everything a lane reads from memory other than the edges it prepares is loaded here, at the same
  // time as the edges, and not behind the group syncs: the residual, the weights of the smooth
  // predictors, the inter prediction and mask of an inter-intra block, the edge an exact vertical or
  // horizontal predictor copies, and the samples an intra block copy reads. Behind the last sync a lane
  // then only reads group memory, computes and stores, and a wavefront step waits for two loads in a
  // row (the record, then the rest) instead of four.
#ifdef INTRA_8BIT
  const int addr = corner + 4 + x0 + (y + 1) * stride;
#else
  const int addr = corner + 4 + (x0 << 1) + (y + 1) * stride;
#endif
  int2 res = 0;
  [branch] if (block.y & (1 << 5)) {
    res = residuals.Load2(plane_cb.w + ((bx + x0) << 1) + (by + y) * plane_cb.z);
  }
  uint wedge = 0;
  uint2 inter = uint2(0, 0);
  [branch] if (block.y & 0x80000000) {  // inter-intra
    wedge = wedge_mask.Load((block.z << 6) + x0 + y * bw);
#ifdef INTRA_8BIT
    inter = unpack_samples(dst_frame.Load(addr));
#else
    inter = dst_frame.Load2(addr);
#endif
  }
  int smooth_v_weight = 0;
  int4 smooth_h_weights = 0;
  if (!directional && mode < MODE_PAETH) {
    if (mode_info & SmoothV) smooth_v_weight = cb_sm_weight_arrays[bh + y].x;
    if (mode_info & SmoothH)
      smooth_h_weights = int4(cb_sm_weight_arrays[bw + x0].x, cb_sm_weight_arrays[bw + x0 + 1].x,
                              cb_sm_weight_arrays[bw + x0 + 2].x, cb_sm_weight_arrays[bw + x0 + 3].x);
  }
  uint2 early_pixels = uint2(0, 0);
  [branch] if (straight) {
    early_pixels = predict_straight(mode == 0, corner, stride, x0, y, above_available, left_available);
  } else if (mode == MODE_INTRA_BC) {
    early_pixels = predict_intra_bc(block, plane_cb, stride, bx + x0, by + y);
  }
#endif

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
      EDGE(EDGE_ABOVE, raw_above + wi) = sum;
    }
    if (cfl) {
      luma = cfl_luma_row(block.z, bx, by, x0, y);
      EDGE(EDGE_LEFT, raw_left + wi) = luma.x + luma.y + luma.z + luma.w;
    }
  } else if (mode < MODE_INTRA_BC && !straight && active) {
    int first_above = 0;
    int first_left = 0;
#ifdef INTRA_SINGLE_EDGE
    // Zones 1 and 3 use the opposite edge to supply a missing primary edge.
    // With both present they use the corner sample itself; only zone 2 filters it.
    const bool read_first_above = mode_info != Dir3 || left_available == 0;
    const bool read_first_left = mode_info != Dir1 || above_available == 0;
    if (wi < (above_count >> 1) || (wi == 0 && read_first_above)) {
#else
    if (wi < (above_count >> 1) || wi == 0) {
#endif
      uint pair = (PIXEL_MID - 1) | ((PIXEL_MID - 1) << 16);
      if (above_available) {
        pair = load_above_pair(corner, wi, above_available);
      } else if (left_available) {
        const uint p = load_left(corner, stride, 0, left_available) & 1023;
        pair = p | (p << 16);
      }
      first_above = pair & 1023;
      EDGE(EDGE_ABOVE, raw_above + wi * 2) = first_above;
      EDGE(EDGE_ABOVE, raw_above + wi * 2 + 1) = (pair >> 16) & 1023;
    }
#ifdef INTRA_SINGLE_EDGE
    if (wi < left_count || (wi == 0 && read_first_left)) {
#else
    if (wi < left_count || wi == 0) {
#endif
      first_left = PIXEL_MID + 1;
      if (left_available) {
        first_left = load_left(corner, stride, wi, left_available);
      } else if (above_available) {
        // The missing column repeats the first above pixel. Load it directly to avoid a group sync.
        first_left = wi == 0 ? first_above : dst_frame.Load(corner + 4) & PIXEL_MAX;
      }
      EDGE(EDGE_LEFT, raw_left + wi) = first_left;
    }
    if (wi + wi_count < left_count) {
      int value = first_left;
      if (left_available) value = load_left(corner, stride, wi + wi_count, left_available);
      EDGE(EDGE_LEFT, raw_left + wi + wi_count) = value;
    }

    if (wi == 0 && need_aboveleft) {
      int topleft = left_available ? first_left : above_available ? first_above : PIXEL_MID;
      if (left_available && above_available) topleft = dst_frame.Load(corner) >> LEFT_SHIFT;
      if ((mode_flags & FilterAboveLeftFlag) && bw + bh >= 24 && cb_flags.x)
        topleft = ((first_left + first_above) * 5 + topleft * 6 + 8) >> 4;
      EDGE(EDGE_ABOVE, raw_above - 1) = topleft;
      EDGE(EDGE_ABOVE, raw_above - 2) = topleft;
      EDGE(EDGE_LEFT, raw_left - 1) = topleft;
      EDGE(EDGE_LEFT, raw_left - 2) = topleft;
    }
  }

  GroupMemoryBarrierWithGroupSync();

#ifndef INTRA_PREFETCH
  int4 next_above = 0;
  int4 next_left = 0;
  if (directional) {
    next_above = prepare_edge(EDGE_ABOVE, raw_above, wi, wi_count, above_count, upsample_above,
                              filter_above, strength_above, need_aboveleft);
    next_left = prepare_edge(EDGE_LEFT, raw_left, wi, wi_count, left_count, upsample_left,
                             filter_left, strength_left, need_aboveleft);
#else
  if (directional) {
    prepare_bank(raw_above, prepared_above, wi, wi_count, above_count, upsample_above,
                 filter_above, strength_above, need_aboveleft);
    prepare_bank(raw_left, prepared_left, wi, wi_count, left_count, upsample_left,
                 filter_left, strength_left, need_aboveleft);
#endif
#ifdef INTRA_MAIN_LEGACY
  } else if (mode >= MODE_DC && wi == 0) {
    const uint count = dc_above * 2 + dc_left;
    uint sum = count >> 1;
    [loop] for (int i = 0; i < dc_terms; ++i) sum += EDGE(EDGE_ABOVE, raw_above + i);
    EDGE(EDGE_ABOVE, raw_above - 1) = count ? sum / count : 512;

    if (cfl) {
      int total = 0;
      [loop] for (int i = 0; i < wi_count; ++i) total += EDGE(EDGE_LEFT, raw_left + i);
      EDGE(EDGE_LEFT, raw_left - 1) = (total + (2 << bsize_log)) >> (bsize_log + 2);
    }
#else
  } else if (mode >= MODE_DC) {
    const int step = reduction_stride(raw_above, wi, wi_count, cfl ? wi_count : dc_terms);
    if (step > 1 || wi == 0) {
      const uint count = dc_above * 2 + dc_left;
      const uint dc = sum_partials(EDGE_ABOVE, raw_above, wi, dc_terms, step);
      if (wi == 0) EDGE(EDGE_ABOVE, raw_above - 1) = count ? (dc + (count >> 1)) / count : PIXEL_MID;

      if (cfl) {
        const int total = sum_partials(EDGE_LEFT, raw_left, wi, wi_count, step);
        if (wi == 0) EDGE(EDGE_LEFT, raw_left - 1) = (total + (2 << bsize_log)) >> (bsize_log + 2);
      }
    }
#endif
  }

#ifndef INTRA_PREFETCH
  GroupMemoryBarrierWithGroupSync();
  if (directional) {
    write_edge(EDGE_ABOVE, raw_above, wi, wi_count, above_count, upsample_above,
               filter_above, strength_above, next_above);
    write_edge(EDGE_LEFT, raw_left, wi, wi_count, left_count, upsample_left,
               filter_left, strength_left, next_left);
  }
#endif

  GroupMemoryBarrierWithGroupSync();
  if (!active) return;

  uint2 pixels = uint2(0, 0);
#ifdef INTRA_PREFETCH
  if (mode == MODE_INTRA_BC || straight) pixels = early_pixels;
#else
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

#endif

#ifdef INTRA_PREFETCH
  if (straight) {
  } else if (mode >= MODE_DC) {
#else
  if (straight) {
    pixels = predict_straight(mode == 0, corner, stride, x0, y, above_available, left_available);
  } else if (mode >= MODE_DC) {
#endif
    const uint dc = EDGE(EDGE_ABOVE, raw_above - 1);
    if (cfl) {
      const int avrg = EDGE(EDGE_LEFT, raw_left - 1);
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
#ifdef INTRA_PREFETCH
        const int edge_base = from_above ? prepared_above : prepared_left;
#endif
        const int p0 = EDGE(edge, edge_base + clamp(base, -2, last));
        int val = p0;
        if (fraction) {
          const int p1 = EDGE(edge, edge_base + clamp(base + 1, -2, last));
          val = p0 + ((fraction * (p1 - p0) + 16) >> 5);
        }
        const uint packed = (uint)clamp(val, 0, PIXEL_MAX) << ((i & 1) * 16);
        if (i < 2) pixels.x |= packed;
        else pixels.y |= packed;
      }
    } else if (mode == MODE_PAETH) {
      const int left = EDGE(EDGE_LEFT, raw_left + y);
      const int corner_pixel = EDGE(EDGE_ABOVE, raw_above - 1);
      const int diff_top = corner_pixel - left;
      const int dt = abs(diff_top);
      [loop] for (int i = 0; i < 4; ++i) {
        const int top = EDGE(EDGE_ABOVE, raw_above + x0 + i);
        const int diff_left = corner_pixel - top;
        const int diff_corner = abs(diff_top + diff_left);
        const int dl = abs(diff_left);
        const int val = dl <= dt && dl <= diff_corner ? left : dt <= diff_corner ? top : corner_pixel;
        const uint packed = (uint)clamp(val, 0, PIXEL_MAX) << ((i & 1) * 16);
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
#ifdef INTRA_PREFETCH
        vertical_weight = smooth_v_weight;
#else
        vertical_weight = cb_sm_weight_arrays[bh + y].x;
#endif
        bottom_term = (256 - vertical_weight) * EDGE(EDGE_LEFT, raw_left + bh - 1);
      }
      if (mode_info & SmoothH) {
        left = EDGE(EDGE_LEFT, raw_left + y);
        right = EDGE(EDGE_ABOVE, raw_above + bw - 1);
      }
      [loop] for (int i = 0; i < 4; ++i) {
        const int x = x0 + i;
        int sum = bottom_term;
        if (mode_info & SmoothV) sum += vertical_weight * EDGE(EDGE_ABOVE, raw_above + x);
#ifdef INTRA_PREFETCH
        if (mode_info & SmoothH) sum += 256 * right + smooth_h_weights[i] * (left - right);
#else
        if (mode_info & SmoothH) sum += 256 * right + cb_sm_weight_arrays[bw + x].x * (left - right);
#endif
        const uint packed = (uint)clamp((sum + (1 << (params.w - 1))) >> params.w, 0, PIXEL_MAX) << ((i & 1) * 16);
        if (i < 2) pixels.x |= packed;
        else pixels.y |= packed;
      }
    }
  }

#ifdef INTRA_PREFETCH
  if (block.y & 0x80000000)  // inter-intra
  {
#else
  const int addr = corner + 4 + (x0 << 1) + (y + 1) * stride;
  if (block.y & 0x80000000)  // inter-intra
  {
    int wedge_addr = (block.z << 6) + x0 + y * bw;
    uint wedge = wedge_mask.Load(wedge_addr);
    uint2 inter = dst_frame.Load2(addr);
#endif

    pixels.x = intra_inter_blend((wedge >> 0) & 255, (pixels.x >> 0) & 1023, (inter.x >> 0) & 1023) |
               (intra_inter_blend((wedge >> 8) & 255, (pixels.x >> 16) & 1023, (inter.x >> 16) & 1023) << 16);
    pixels.y = intra_inter_blend((wedge >> 16) & 255, (pixels.y >> 0) & 1023, (inter.y >> 0) & 1023) |
               (intra_inter_blend((wedge >> 24) & 255, (pixels.y >> 16) & 1023, (inter.y >> 16) & 1023) << 16);
  }

  if (block.y & (1 << 5)) {
#ifndef INTRA_PREFETCH
    const int res_stride = cb_planes[plane].z;
    const int res_addr = cb_planes[plane].w + ((bx + x0) << 1) + (by + y) * res_stride;
    int2 res = residuals.Load2(res_addr);
#endif
    pixels.x = clamp((int)((pixels.x >> 0) & 1023) + (int)((res.x << 16) >> 16), 0, PIXEL_MAX) |
               (clamp((int)((pixels.x >> 16) & 1023) + (int)(res.x >> 16), 0, PIXEL_MAX) << 16);
    pixels.y = clamp((int)((pixels.y >> 0) & 1023) + (int)((res.y << 16) >> 16), 0, PIXEL_MAX) |
               (clamp((int)((pixels.y >> 16) & 1023) + (int)(res.y >> 16), 0, PIXEL_MAX) << 16);
  }

#ifdef INTRA_8BIT
  dst_frame.Store(addr, pack_samples(pixels));
#else
  dst_frame.Store2(addr, pixels);
#endif
}

#ifndef INTRA_RUN
[numthreads(WG_SIZE, 1, 1)] void main(uint3 thread : SV_DispatchThreadID) {
  intra_main_body(thread.x, thread.x & (WG_SIZE - 1), cb_counts0, cb_counts1, cb_wi_count, cb_pass_offset);
}
#else
// AV1 7.11.2: a block is predicted from the samples of the blocks above and to the left of it, which are
// in earlier iterations of the wavefront. The run is one thread group for iterations so narrow that the
// dispatch and barrier of a step of their own (about 18 us on the console) cost more than the group takes
// for them: the group runs them one after the other, an iteration's lanes in chunks of GROUP_THREADS,
// with a device memory barrier between iterations, so that an iteration reads what the group wrote in the
// ones before. No group waits for another: the run is one group. An iteration of the run has no filter
// intra blocks (the host ends a run in front of one).
[numthreads(GROUP_THREADS, 1, 1)] void main(uint3 group_thread : SV_GroupThreadID) {
  const int local = (int)group_thread.x;
  for (uint it = 0; it < cb_run_iters; ++it) {
    // The starts of the iteration's ten lists and of the next iteration's (IntraTypeCount 10), the same for
    // every thread of the group.
    const uint at = (cb_run_lists + it * 10) * 4;
    const uint4 starts0 = block_map.Load4(at);
    const uint4 starts1 = block_map.Load4(at + 16);
    const uint start8 = block_map.Load(at + 32);
    const uint start9 = block_map.Load(at + 36);
    const int4 counts0 = int4((int)(starts0.y - starts0.x), (int)(starts0.z - starts0.y), (int)(starts0.w - starts0.z),
                              (int)(starts1.x - starts0.w));
    const int4 counts1 = int4((int)(starts1.y - starts1.x), (int)(starts1.z - starts1.y), (int)(starts1.w - starts1.z),
                              (int)(start8 - starts1.w));
    // Lanes of four samples of a row: 1024 in a 64x64 block down to 4 in a 4x4 (prediction.rs).
    const uint total = ((uint)counts0.x << 10) + ((uint)counts0.y << 9) + ((uint)counts0.z << 8) +
                       ((uint)counts0.w << 7) + ((uint)counts1.x << 6) + ((uint)counts1.y << 5) +
                       ((uint)counts1.z << 4) + ((uint)counts1.w << 3) + ((start9 - start8) << 2);
    [loop] for (uint first = 0; first < total; first += GROUP_THREADS) {
      intra_main_body(first + local, local, counts0, counts1, total, (int)starts0.x);
    }
    DeviceMemoryBarrierWithGroupSync();
  }
}
#endif
