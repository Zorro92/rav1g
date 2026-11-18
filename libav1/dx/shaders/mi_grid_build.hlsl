/*
 * SPDX-License-Identifier: BSD-2-Clause
 *
 * This source code is subject to the terms of the BSD 2 Clause License and the Alliance for Open Media
 * Patent License 1.0. If the BSD 2 Clause License was not distributed with this source code in the LICENSE
 * file, you can obtain it at www.aomedia.org/license/software. If the Alliance for Open Media Patent
 * License 1.0 was not distributed with this source code in the PATENTS file, you can obtain it at
 * www.aomedia.org/license/patent.
 */

// The mode info grid of a frame, from the frame's mode info records: for every 4x4 unit of the frame the
// entry of the record of the block that covers it, as the sink writes it on the parse threads (block_call
// of crates/rav1g-parse/src/sink.rs): where the record starts in mi_pool, in bytes, plus cb_mi_addr_base,
// at every unit of the block that is inside the frame. The blocks of a frame do not overlap, so every unit
// has one writer. The pass writes the grid of the slot in a decoder whose sink writes none
// (AV1_PARSER_OPTION_GPU_GRID), in front of every stage that reads it; with the check
// (AV1_PARSER_OPTION_GPU_GRID_CHECK) a second grid that the host holds against the sink's
// (crates/rav1g/src/host/grid_check.rs).
//
// One thread a record. The frame's records lie in runs of the pool (the pieces its tiles took): runs has
// for each run the index of its first record in the pool and the records of the runs in front of it, so
// thread i finds its run by a search over those sums.

ByteAddressBuffer mi_pool : register(t0);
ByteAddressBuffer runs : register(t1);
RWByteAddressBuffer grid : register(u0);

cbuffer cb_mi_grid_build : register(b0) {
  uint cb_run_count;
  uint cb_records;       // the records of all runs
  uint cb_mi_stride;
  uint cb_mi_cols;
  uint cb_mi_rows;
  uint cb_mi_addr_base;  // what an entry has above the place of its record (MI_GRID_BASE)
  uint cb_record_bytes;  // the size of a record (MB_MODE_INFO of mode_info.h, 224 bytes)
};

// The fields of a record the pass reads (offsets of MB_MODE_INFO, held by the host).
#define MI_ROW_BYTE 160
#define MI_COL_BYTE 164
#define SB_TYPE_WORD 188  // byte 0: sb_type, a BLOCK_SIZE

// log2 of the width and the height in 4x4 units, by BLOCK_SIZE (mi_size_wide_log2, mi_size_high_log2).
static const uint kWideLog2[22] = {0, 0, 1, 1, 1, 2, 2, 2, 3, 3, 3, 4, 4, 4, 5, 5, 0, 2, 1, 3, 2, 4};
static const uint kHighLog2[22] = {0, 1, 0, 1, 2, 1, 2, 3, 2, 3, 4, 3, 4, 5, 4, 5, 2, 0, 3, 1, 4, 2};

[numthreads(64, 1, 1)] void main(uint3 thread : SV_DispatchThreadID) {
  const uint i = thread.x;
  if (i >= cb_records) return;
  // The last run whose first record is at or in front of record i of the frame.
  uint lo = 0;
  uint hi = cb_run_count;
  while (hi - lo > 1) {
    const uint mid = (lo + hi) >> 1;
    if (runs.Load(mid * 8 + 4) <= i) {
      lo = mid;
    } else {
      hi = mid;
    }
  }
  const uint index = runs.Load(lo * 8) + (i - runs.Load(lo * 8 + 4));
  const uint base = index * cb_record_bytes;
  const uint row = mi_pool.Load(base + MI_ROW_BYTE);
  const uint col = mi_pool.Load(base + MI_COL_BYTE);
  const uint bsize = min(mi_pool.Load(base + SB_TYPE_WORD) & 255, 21);
  const uint x_mis = min(1u << kWideLog2[bsize], cb_mi_cols - col);
  const uint y_mis = min(1u << kHighLog2[bsize], cb_mi_rows - row);
  const uint entry = base + cb_mi_addr_base;
  for (uint y = 0; y < y_mis; ++y) {
    const uint at = ((row + y) * cb_mi_stride + col) * 4;
    for (uint x = 0; x < x_mis; ++x) {
      grid.Store(at + x * 4, entry);
    }
  }
}
