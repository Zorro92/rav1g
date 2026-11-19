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

RWByteAddressBuffer dst_frame : register(u0);
ByteAddressBuffer lf_blocks : register(t0);

cbuffer LoopfilterData : register(b0) {
  int4 cb_planes[3];
  int4 cb_limits[64];
};

cbuffer LoopfilterSRT : register(b1) {
  uint cb_wicount;
  uint cb_plane;
  uint cb_offset_base;
  uint cb_block_cols;
  uint cb_block_id_offset;
};

// Keep the type and the unscaled AV1 thresholds in one word. The inside limit is at most 63,
// the macroblock limit at most 193, and the HEV threshold at most 3.
uint prepare_lf_edge(uint edge) {
  uint level = edge & 63;
  if (level == 0) return 0;
  int4 limits = cb_limits[level];
  return ((edge >> 6) & 3) | (limits.x << 2) | (limits.y << 8) | (limits.z << 16);
}

int4 lf_edge_limits(uint edge) {
  return int4((edge >> 2) & 63, (edge >> 8) & 255, edge >> 16, 0) << LfLimitShift;
}
