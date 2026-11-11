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

// Scheduling for intra_v2.hlsl (test player flag -intrav2). Shared by the
// decoder (prediction.cpp) and the host check (testing/intra_check).
//
// The original path gives every intra block a wavefront iteration of the
// whole frame and runs one dispatch pair per iteration. Here the frame is cut
// into 64x64 luma regions (16x16 cells of 4x4 pixels, plus the 8x8 chroma
// cells below them). A block gets
//   - a step: the dispatch round. All blocks of a region with the same step
//     are handled by one thread group. A block's step is higher than the step
//     of every block it reads from another region, so the host needs one UAV
//     barrier per step and groups never wait for each other;
//   - a pass: the round inside the thread group, higher than the pass of every
//     block it reads in its own group. A filter intra block takes one pass per
//     anti-diagonal of its cells.
// Blocks are placed in decode order, like the iteration grid of the original.

#ifndef AV1_DX_INTRA_V2_SCHED_H_
#define AV1_DX_INTRA_V2_SCHED_H_

#include <stdint.h>

#include <algorithm>

#if defined(_MSC_VER)
#include <intrin.h>
#define INTRA_V2_NEXT_ENTRY(counter) ((uint32_t)_InterlockedIncrement(counter) - 1)
#else
#define INTRA_V2_NEXT_ENTRY(counter) ((uint32_t)__sync_fetch_and_add(counter, 1))
#endif

// Passes of one thread group; a region with a longer chain of blocks goes on
// in a later step. The last pass of a block is stored in 8 bits.
#ifndef INTRA_V2_MAX_PASSES
#define INTRA_V2_MAX_PASSES 250
#endif

enum {
  IntraV2FilterBucket = 9,  // block type of filter intra blocks (0..8: 64x64 .. 4x4 by area)
  IntraV2MaxPasses = INTRA_V2_MAX_PASSES,
  IntraV2MaxRegions = 256,  // regions per row or column (8 bits each in a region entry)
};

// Not std::max: <windows.h> may define a max() macro.
static inline uint32_t intra_v2_max(uint32_t a, uint32_t b) { return a > b ? a : b; }

// One frame's schedule. Tiles are decoded on several threads: cells and
// regions of different tiles are disjoint, the region list is appended to
// through an interlocked counter.
typedef struct IntraV2Sched {
  // GPU, 2 words per 4x4 cell, luma cells first, then the chroma cells
  // (shared by U and V):
  //   [0] block index in its type (24 bits) | block type << 24
  //   [1] step (16 bits) | last pass of the block << 16 | stamp << 24
  uint32_t *cells;
  // CPU, 2 words per region: [0] index of its current list entry,
  // [1] step of that entry | stamp << 24.
  uint32_t *regions;
  // GPU, one entry per thread group:
  //   low word:  first intra block type of the tile in the block map (24 bits) | region column << 24
  //   high word: region row | last pass << 8 | step << 16
  uint64_t *list;
  volatile long *list_count;
  int map_stride;      // luma cells per row
  int map_uv_offset;   // index of the first chroma cell
  int regions_stride;  // regions per row
  uint32_t stamp;      // 1..255: marks the cells and regions written for this frame
} IntraV2Sched;

// Moves on to the stamp of the next frame. Returns nonzero when the cell map
// and the regions have to be cleared first: for the first frame of a buffer
// (stamp 0) and whenever the 8-bit stamp starts over.
static inline int intra_v2_next_stamp(uint32_t *stamp) {
  *stamp = (*stamp % 255) + 1;
  return *stamp == 1;
}

// Places one transform block. (cx, cy) and the sizes are in 4x4 cells of the
// block's plane. The dependencies are the cells the original scans in its
// iteration grid: above_count cells of the row above (one more to the left
// with above_left), left_count cells of the column to the left, and for CFL
// the luma cells under the block. `index` is the block's index in its type
// (and tile); U and V blocks are placed once, with the index of U.
static inline void intra_v2_place(const IntraV2Sched *s, uint32_t tile_base, int is_uv, int cx, int cy, int w_log,
                                  int h_log, int above_left, int above_count, int left_count, int cfl,
                                  uint32_t bucket, uint32_t index) {
  const int shift = 4 - is_uv;  // log2 of the cells per region row
  const int stride = s->map_stride >> is_uv;
  uint32_t *const map = s->cells + (is_uv ? 2 * (size_t)s->map_uv_offset : 0);
  const int rx = cx >> shift;
  const int ry = cy >> shift;
  uint32_t *const region = s->regions + 2 * (rx + ry * s->regions_stride);
  const uint32_t stamp = s->stamp << 24;
  const int open = (region[1] & 0xff000000) == stamp;
  const uint32_t cur = region[1] & 0xffff;
  uint32_t need_step = 0;
  uint32_t need_pass = 0;

  if (above_count + above_left > 0) {
    const uint32_t *row = map + 2 * ((size_t)(cy - 1) * stride) + 1;
    const int same_row = ((cy - 1) >> shift) == ry;
    for (int x = cx - above_left; x < cx + above_count; ++x) {
      const uint32_t d = row[2 * x];
      if ((d & 0xff000000) != stamp) continue;
      const uint32_t step = d & 0xffff;
      if (same_row && (x >> shift) == rx) {
        if (step == cur) need_pass = intra_v2_max(need_pass, ((d >> 16) & 255) + 1);
      } else {
        need_step = intra_v2_max(need_step, step + 1);
      }
    }
  }
  if (left_count > 0) {
    const int same_col = ((cx - 1) >> shift) == rx;
    for (int y = cy; y < cy + left_count; ++y) {
      const uint32_t d = map[2 * ((size_t)y * stride + cx - 1) + 1];
      if ((d & 0xff000000) != stamp) continue;
      const uint32_t step = d & 0xffff;
      if (same_col && (y >> shift) == ry) {
        if (step == cur) need_pass = intra_v2_max(need_pass, ((d >> 16) & 255) + 1);
      } else {
        need_step = intra_v2_max(need_step, step + 1);
      }
    }
  }
  if (cfl) {
    // The luma cells under a chroma block are in the same region.
    for (int y = cy << 1; y < ((cy << 1) + (2 << h_log)); ++y) {
      const uint32_t *row = s->cells + 2 * ((size_t)y * s->map_stride + (cx << 1)) + 1;
      for (int x = 0; x < (2 << w_log); ++x) {
        const uint32_t d = row[2 * x];
        if ((d & 0xff000000) == stamp && (d & 0xffff) == cur) need_pass = intra_v2_max(need_pass, ((d >> 16) & 255) + 1);
      }
    }
  }

  const uint32_t duration = (bucket == IntraV2FilterBucket) ? (uint32_t)((1 << w_log) + (1 << h_log) - 2) : 0;
  uint32_t step = cur;
  uint32_t pass = need_pass;
  int new_entry = 0;
  if (!open || need_step > cur) {
    step = need_step;
    pass = 0;
    new_entry = 1;
  } else if (pass > 0 && pass + duration >= IntraV2MaxPasses) {
    step = cur + 1;
    pass = 0;
    new_entry = 1;
  }
  if (step > 0xffff) step = 0xffff;  // more steps than list entries of any real frame
  if (new_entry) {
    region[0] = INTRA_V2_NEXT_ENTRY(s->list_count);
    region[1] = step | stamp;
    s->list[region[0]] = (uint64_t)(tile_base | ((uint32_t)rx << 24)) | ((uint64_t)((uint32_t)ry | (step << 16)) << 32);
  }
  const uint32_t last_pass = pass + duration;
  uint64_t *const entry = s->list + region[0];
  if (last_pass > ((*entry >> 40) & 255)) *entry = (*entry & ~((uint64_t)0xff << 40)) | ((uint64_t)last_pass << 40);

  const uint32_t word0 = index | (bucket << 24);
  const uint32_t word1 = step | (last_pass << 16) | stamp;
  for (int y = cy; y < cy + (1 << h_log); ++y) {
    uint32_t *cell = map + 2 * ((size_t)y * stride + cx);
    for (int x = 0; x < (1 << w_log); ++x) {
      cell[2 * x] = word0;
      cell[2 * x + 1] = word1;
    }
  }
}

// Orders the region list by step, then by number of passes, for
// intra_v2_next_dispatch().
static inline void intra_v2_sort(uint64_t *list, int count) { std::sort(list, list + count); }

typedef struct IntraV2Dispatch {
  int first;       // first region entry
  int count;       // thread groups
  uint32_t step;
  uint32_t passes;
  int last_of_step;  // a UAV barrier has to follow
} IntraV2Dispatch;

// Cuts the sorted list into dispatches: all thread groups of a dispatch run
// the pass count of its busiest region, so regions of one step with very
// different pass counts are dispatched separately. Returns 0 at the end.
static inline int intra_v2_next_dispatch(const uint64_t *list, int count, int first, IntraV2Dispatch *d) {
  if (first >= count) return 0;
  const uint32_t step = (uint32_t)(list[first] >> 48);
  const uint32_t limit = 2 * ((uint32_t)(list[first] >> 40) & 255) + 6;
  int end = first + 1;
  while (end < count && (uint32_t)(list[end] >> 48) == step && ((uint32_t)(list[end] >> 40) & 255) + 1 <= limit) ++end;
  d->first = first;
  d->count = end - first;
  d->step = step;
  d->passes = ((uint32_t)(list[end - 1] >> 40) & 255) + 1;
  d->last_of_step = end >= count || (uint32_t)(list[end] >> 48) != step;
  return 1;
}

#endif  // AV1_DX_INTRA_V2_SCHED_H_
