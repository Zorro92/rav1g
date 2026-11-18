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

#include "pred_list_common.h"

ByteAddressBuffer templates : register(t0);
RWByteAddressBuffer scratch : register(u0);

cbuffer ScanData : register(b0) {
  uint cb_groups;
  uint cb_lists;
  uint cb_starts_offset;
  uint cb_counts_offset;
  uint cb_args_offset;
  uint cb_template_offset;
  uint cb_intra_records;
};

#define ScanThreads 128
#define ListsPerChunk 8
groupshared uint totals[PredMaxLists + 1];
groupshared uint starts[PredMaxLists + 1];
groupshared uint chunks[ScanThreads];

[numthreads(ScanThreads, 1, 1)]
void main(uint lane : SV_GroupIndex) {
  [loop] for (uint list = lane; list < cb_lists; list += ScanThreads) {
    uint total = 0;
    [loop] for (uint group = 0; group < cb_groups; ++group) {
      uint address = (group * cb_lists + list) * 4;
      uint count = scratch.Load(address);
      scratch.Store(address, total);
      total += count;
    }
    totals[list] = total;
  }
  if (lane == 0) totals[cb_lists] = 0;  // a sentinel for the final prefix
  GroupMemoryBarrierWithGroupSync();

  const uint first = lane * ListsPerChunk;
  if (first <= cb_lists) {
    uint total = 0;
    [loop] for (uint i = first; i <= cb_lists && i < first + ListsPerChunk; ++i) {
      starts[i] = total;
      total += totals[i];
    }
    chunks[lane] = total;
  }
  GroupMemoryBarrierWithGroupSync();

  if (first <= cb_lists) {
    uint prefix = 0;
    [loop] for (uint chunk = 0; chunk < lane; ++chunk) prefix += chunks[chunk];
    [loop] for (uint i = first; i <= cb_lists && i < first + ListsPerChunk; ++i) starts[i] += prefix;
  }
  GroupMemoryBarrierWithGroupSync();

  // The warp buffer starts at zero. Fast lists follow the intra records in
  // the ordinary buffer, after the inter and reconstruction lists.
  uint intra_base = starts[PredListWarpBlock];
  uint warp_end = starts[PredListFast];
  uint warp_total = warp_end - intra_base;
  if (lane == 0) {
    scratch.Store3(cb_starts_offset, uint3(starts[cb_lists] - warp_total + cb_intra_records, warp_total, intra_base));
  }
  [loop] for (uint list_id = lane; list_id < cb_lists; list_id += ScanThreads) {
    uint start = starts[list_id];
    if (list_id >= PredListFast) start = start - warp_total + cb_intra_records;
    else if (list_id >= PredListWarpBlock) start -= intra_base;
    scratch.Store(cb_starts_offset + PredHeaderBytes + list_id * 4, start);
  }

  // One thread per batch. For inter/reconstruction, skip empty lists while
  // writing the commands and set the ExecuteIndirect count to what remains.
  // For intra the CPU already knows the counts and sizes. Patch just the
  // constant holding the relative record offset.
  uint batches = templates.Load(0);
  [loop] for (uint batch = lane; batch < batches; batch += ScanThreads) {
    uint4 desc = templates.Load4(16 + batch * 16);
    uint written = 0;
    [loop] for (uint i = 0; i < desc.y; ++i) {
      uint4 words = templates.Load4(cb_template_offset + (desc.x + i) * 16);
      uint groups = desc.w;
      if (desc.z == 0) {
        uint list_id = words.x;
        uint count = totals[list_id];
        if (count == 0) continue;
        uint start = starts[list_id];
        if (list_id >= PredListFast) start = start - warp_total + cb_intra_records;
        else if (list_id >= PredListWarpBlock) start -= intra_base;
        words.x = count * words.y;
        words.y = start;
        groups = (words.x + 63) >> 6;
      } else {
        if (desc.z == 2) words.y += intra_base;
        else if (desc.z == 3) words.z += intra_base;
        else words.w += intra_base;
      }

      uint address = cb_args_offset + (desc.x + written) * 28;
      scratch.Store4(address, words);
      scratch.Store3(address + 16, uint3(groups, 1, 1));
      ++written;
    }
    scratch.Store(cb_counts_offset + batch * 4, written);
  }
}
