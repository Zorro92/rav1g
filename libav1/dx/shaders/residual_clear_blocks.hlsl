/*
 * SPDX-License-Identifier: BSD-2-Clause
 *
 * This source code is subject to the terms of the BSD 2 Clause License and the Alliance for Open Media
 * Patent License 1.0. If the BSD 2 Clause License was not distributed with this source code in the LICENSE
 * file, you can obtain it at www.aomedia.org/license/software. If the Alliance for Open Media Patent
 * License 1.0 was not distributed with this source code in the PATENTS file, you can obtain it at
 * www.aomedia.org/license/patent.
 */

// Zeroes only the transform blocks that the frame's inverse transform wrote into `idct_residuals`,
// running after prediction so the residual buffer returns to all zeros before the next frame without
// touching skipped/unwritten regions (saving most of the 25.56 MB per-frame clear on 4K frames).
//
// Reads the sorted transform block records (`buf_blocks`) and the frame's residual plane layout
// (`cb_idct_frame_data`) that `idct_run` already bound. Zero LDS, coalesced 16-byte stores (8-byte
// for 4-wide transforms), power-of-two lanes per block so every lane issues at most 8 stores.

#define IdctBlockSize 16

ByteAddressBuffer buf_blocks : register(t0);
RWByteAddressBuffer buf_dst : register(u0);

cbuffer cb_idct_frame_data : register(b0) {
  uint4 cb_planes[3];
  int cb_bitdepth;
};

cbuffer cb_clear_dispatch : register(b1) {
  uint cb_index_offset;     // first record in buf_blocks for this tx_type
  uint cb_total_lanes;      // block_count << lanes_shift
  uint cb_lanes_shift;      // log2(lanes_per_block), in 0..6
  uint cb_packed_dims;      // bits 0..7: tx_w (4..64), bits 8..15: tx_h (4..64), bits 16..23: col_shift (0..3)
};

[numthreads(64, 1, 1)] void main(uint3 thread : SV_DispatchThreadID) {
  if (thread.x >= cb_total_lanes) return;

  const uint block_idx = thread.x >> cb_lanes_shift;
  const uint lanes_per_block = 1u << cb_lanes_shift;
  const uint wi = thread.x & (lanes_per_block - 1u);

  const uint addr = (cb_index_offset + block_idx) * IdctBlockSize;
  const uint bx = buf_blocks.Load(addr);
  const uint bz = buf_blocks.Load(addr + 8u);

  const uint plane = (bx >> 21u) & 3u;
  const uint stride = cb_planes[plane].x;
  const uint origin = cb_planes[plane].y + 8u * (bz & 0xffffu) + 4u * (bz >> 16u) * stride;

  const uint tx_w = cb_packed_dims & 0xffu;
  const uint tx_h = (cb_packed_dims >> 8u) & 0xffu;
  const uint col_shift = (cb_packed_dims >> 16u) & 0xffu;

  if (tx_w == 4u) {
    // 4 samples = 8 bytes per row (Store2).
    for (uint r = wi; r < tx_h; r += lanes_per_block) {
      buf_dst.Store2(origin + r * stride, uint2(0u, 0u));
    }
  } else {
    // tx_w in {8, 16, 32, 64} -> (1 << col_shift) Store4s (16 bytes each) per row.
    const uint total_stores = tx_h << col_shift;
    const uint col_mask = (1u << col_shift) - 1u;
    for (uint k = wi; k < total_stores; k += lanes_per_block) {
      const uint row = k >> col_shift;
      const uint col = k & col_mask;
      buf_dst.Store4(origin + row * stride + (col << 4u), uint4(0u, 0u, 0u, 0u));
    }
  }
}
