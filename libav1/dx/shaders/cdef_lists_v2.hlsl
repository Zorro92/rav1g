/*
 * SPDX-License-Identifier: BSD-2-Clause
 *
 * This source code is subject to the terms of the BSD 2 Clause License and the Alliance for Open Media
 * Patent License 1.0. If the BSD 2 Clause License was not distributed with this source code in the LICENSE
 * file, you can obtain it at www.aomedia.org/license/software. If the Alliance for Open Media Patent
 * License 1.0 was not distributed with this source code in the PATENTS file, you can obtain it at
 * www.aomedia.org/license/patent.
 */

// Builds the compact CDEF v3 lists from the frame's mode info (AV1 7.15):
//   - cdef_index: one signed strength index per 64x64 luma block (identical to cdef_lists.hlsl)
//   - cdef_masks: one 32-bit active mask per 64x32 CDEF group (bit (ly * 8 + bx) is 1 when the 8x8
//     block is inside the coded frame and at least one of its four 4x4 units is not skipped).
//
// Packing the 32 skip flags of each 64x32 group into one 32-bit word shrinks the skip list 32x
// (from ~2.08 MiB to 16 KiB at 4K) and lets cdef_filter_v3 early-exit all-skipped 64x32 groups
// with a single scalar load before touching LDS or frame halos.
//
// One group per 64x64 block, one thread per 8x8 block. Uses the same root signature as cdef_lists.

#include "mode_info.h"

ByteAddressBuffer mi_pool : register(t0);
ByteAddressBuffer mi_grid : register(t1);
RWByteAddressBuffer cdef_index : register(u0);
RWByteAddressBuffer cdef_masks : register(u1);

cbuffer cb_cdef_lists : register(b0) {
  uint cb_mi_stride;
  uint cb_mi_addr_base;
  uint cb_index_stride;  // 64x64 blocks per row of cdef_index and cdef_masks
  uint cb_skips_stride;  // unused here; kept so the root signature matches cdef_lists.hlsl
  uint cb_skip_cols;     // 8x8 blocks of the frame in X
  uint cb_skip_rows;     // 8x8 blocks of the frame in Y
};

groupshared uint active_bit[64];

// A zero grid entry reads the first record.
uint tx_info(uint entry) {
  const uint record = entry == 0 ? 0u : entry - cb_mi_addr_base;
  return mi_pool.Load(record + TX_INFO_WORD);
}

bool is_skipped(uint word) { return (word & 0xff00u) != 0; }

[numthreads(8, 8, 1)] void main(uint3 group : SV_GroupID, uint3 thread : SV_DispatchThreadID,
                                uint3 local : SV_GroupThreadID, uint tid : SV_GroupIndex) {
  // Clamp loads so out-of-frame threads stay inside the mode-info grid without diverging before sync.
  const uint col = min(thread.x, cb_skip_cols - 1);
  const uint row = min(thread.y, cb_skip_rows - 1);
  const uint at = (row * 2 * cb_mi_stride + col * 2) * 4;
  const uint2 top = mi_grid.Load2(at);
  const uint2 bottom = mi_grid.Load2(at + cb_mi_stride * 4);
  const bool in_bounds = thread.x < cb_skip_cols && thread.y < cb_skip_rows;

  uint bit = 0u;
  uint word = 0u;
  if (in_bounds) {
    word = tx_info(top.x);
    bool skipped = is_skipped(word);
    // A block that covers all four units needs just one mode-info read.
    [branch] if (top.x != top.y || top.x != bottom.x || top.x != bottom.y) {
      const bool top_right = is_skipped(tx_info(top.y));
      const bool bottom_left = is_skipped(tx_info(bottom.x));
      const bool bottom_right = is_skipped(tx_info(bottom.y));
      skipped = skipped && top_right && bottom_left && bottom_right;
    }
    if (!skipped) {
      bit = 1u << ((local.y & 3u) * 8u + local.x);
    }
  }
  active_bit[tid] = bit;

  // The first thread of the 64x64 group writes its strength index (always in bounds).
  if (tid == 0u) {
    const int strength = ((int)(word << 8)) >> 24;
    cdef_index.Store((group.y * cb_index_stride + group.x) * 4u, (uint)strength);
  }

  GroupMemoryBarrierWithGroupSync();

  // Two threads reduce the top (rows 0..3) and bottom (rows 4..7) 64x32 halves into two 32-bit masks.
  if (local.x == 0u && (local.y == 0u || local.y == 4u)) {
    const uint base = local.y * 8u;
    uint mask = 0u;
    [unroll] for (uint i = 0u; i < 32u; i++) {
      mask |= active_bit[base + i];
    }
    const uint half = local.y >> 2u;
    cdef_masks.Store(((group.y * 2u + half) * cb_index_stride + group.x) * 4u, mask);
  }
}
