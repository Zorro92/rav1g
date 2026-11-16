/*
 * SPDX-License-Identifier: BSD-2-Clause
 *
 * This source code is subject to the terms of the BSD 2 Clause License and the Alliance for Open Media
 * Patent License 1.0. If the BSD 2 Clause License was not distributed with this source code in the LICENSE
 * file, you can obtain it at www.aomedia.org/license/software. If the Alliance for Open Media Patent
 * License 1.0 was not distributed with this source code in the PATENTS file, you can obtain it at
 * www.aomedia.org/license/patent.
 */

// The two lists CDEF reads besides the frame (cdef_filter.hlsl, cdef_filter_v2.hlsl), from the mode info
// of the frame: the strength index of every 64x64 luma block, which the parser keeps in the record of the
// block's top left 4x4 unit, and for every 8x8 luma block whether all four of its 4x4 units are skipped.
// The host writes the same two lists on the thread that records the frame (cdef_filter_run of
// crates/rav1g/src/host/postfilter.rs), one word an entry; this pass writes the same words at the same
// places and nothing else, in front of CDEF in the frame's command list (the decoder's switch for it:
// AV1_PARSER_OPTION_GPU_CDEF_LISTS).
//
// One group a 64x64 block, one thread an 8x8 block (thread.x and thread.y count 8x8 blocks of the frame). The
// grid has entries of four bytes (the Rust host): where the record of the unit's block starts in mi_pool, in
// bytes, plus cb_mi_addr_base.

ByteAddressBuffer mi_pool : register(t0);
ByteAddressBuffer mi_grid : register(t1);
RWByteAddressBuffer cdef_index : register(u0);
RWByteAddressBuffer skips : register(u1);

cbuffer cb_cdef_lists : register(b0) {
  uint cb_mi_stride;
  uint cb_mi_addr_base;
  uint cb_index_stride;  // 64x64 blocks a row of cdef_index
  uint cb_skips_stride;  // entries a row of skips: 16 for every 64x64 block of a row, of which 8 are used
  uint cb_skip_cols;     // the 8x8 blocks of the frame that have a skip flag, by
  uint cb_skip_rows;     // rows: what the host's loop writes, and no entry behind it
};

// The words of a record that hold the two fields (MB_MODE_INFO of mode_info.h, 224 bytes).
#define TX_INFO_WORD 200         // byte 1: skip
#define SEG_CDEF_FLAGS_WORD 220  // byte 1: cdef_strength, signed

// Where the record of the block at a 4x4 unit of the frame starts in mi_pool. Every unit inside the frame
// has a block when the frame is parsed; an entry of zeros would be a unit without one, and reads the first
// record of the pool so that the load stays inside it (the host's loop has no such case either).
uint record_at(uint row, uint col) {
  const uint entry = mi_grid.Load((row * cb_mi_stride + col) * 4);
  return entry == 0 ? 0u : entry - cb_mi_addr_base;
}

uint skip_at(uint row, uint col) { return (mi_pool.Load(record_at(row, col) + TX_INFO_WORD) >> 8) & 255; }

[numthreads(8, 8, 1)] void main(uint3 thread : SV_DispatchThreadID) {
  // The compiler makes the loads of a thread without work unconditional: such a thread loads what the
  // last 8x8 block of the frame loads, so that no load leaves the grid.
  const uint col = min(thread.x, cb_skip_cols - 1);
  const uint row = min(thread.y, cb_skip_rows - 1);
  const bool skipped = skip_at(row * 2, col * 2) != 0 && skip_at(row * 2, col * 2 + 1) != 0 &&
                       skip_at(row * 2 + 1, col * 2) != 0 && skip_at(row * 2 + 1, col * 2 + 1) != 0;
  if (thread.x < cb_skip_cols && thread.y < cb_skip_rows) {
    skips.Store((thread.y * cb_skips_stride + thread.x) * 4, skipped ? 1u : 0u);
  }
  // The first thread of a group: the index of its 64x64 block, whose top left unit is inside the frame.
  if ((thread.x & 7) == 0 && (thread.y & 7) == 0) {
    const uint word = mi_pool.Load(record_at(thread.y * 2, thread.x * 2) + SEG_CDEF_FLAGS_WORD);
    const int strength = ((int)(word << 16)) >> 24;
    cdef_index.Store(((thread.y >> 3) * cb_index_stride + (thread.x >> 3)) * 4, (uint)strength);
  }
}
