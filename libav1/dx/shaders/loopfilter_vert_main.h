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

#include "loopfilter_common.h"

groupshared uint input_edges[64 * 4];
groupshared uint edge_masks[64];

[numthreads(64, 1, 1)]
void main(uint3 thread : SV_DispatchThreadID) {
  const uint lane = thread.x & 63;
  const uint row = lane & 3;
  const bool active = thread.x < cb_wicount;
  const uint block_id = ((thread.x >> 2) << 1) + cb_block_id_offset;

  if (active) {
    // One lane prepares four edge descriptors, shared by the four rows of the block. The generator
    // leaves a zero at every position outside its transform walk, so a bitmap can skip all the
    // holes and level-zero edges while still processing the filtered edges in increasing order.
    uint2 pairs = lf_blocks.Load2((cb_offset_base + block_id) * 32 + row * 8);
    uint4 edges = uint4(prepare_lf_edge(pairs.x), prepare_lf_edge(pairs.x >> 16),
                        prepare_lf_edge(pairs.y), prepare_lf_edge(pairs.y >> 16));
    input_edges[lane * 4 + 0] = edges.x;
    input_edges[lane * 4 + 1] = edges.y;
    input_edges[lane * 4 + 2] = edges.z;
    input_edges[lane * 4 + 3] = edges.w;
    uint mask = (edges.x != 0) | ((edges.y != 0) << 1) | ((edges.z != 0) << 2) | ((edges.w != 0) << 3);
    edge_masks[lane] = mask << (row * 4);
  }
  GroupMemoryBarrierWithGroupSync();
  // The host dispatches complete blocks of four rows, so each active row can read all four masks.
  if (!active) return;

  const uint block_lane = lane & ~3;
  uint remaining = edge_masks[block_lane + 0] | edge_masks[block_lane + 1] |
                   edge_masks[block_lane + 2] | edge_masks[block_lane + 3];
  const uint block_x = block_id % cb_block_cols;
  const uint block_y = block_id / cb_block_cols;
  const uint stride = cb_planes[cb_plane].x;
  const uint addr = cb_planes[cb_plane].y + block_x * 64 * LfSampleBytes + (block_y * 4 + row) * stride;

  while (remaining != 0) {
    uint col = firstbitlow(remaining);
    uint edge = input_edges[block_lane * 4 + col];
    filter_vertical_edge(dst_frame, addr + col * 4 * LfSampleBytes, lf_edge_limits(edge), edge & 3);
    remaining &= remaining - 1;
  }
}
