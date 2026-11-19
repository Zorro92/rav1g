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

#include "mode_info.h"

StructuredBuffer<MB_MODE_INFO> mi_pool : register(t0);
ByteAddressBuffer mi_grid : register(t1);
RWByteAddressBuffer dst_buf : register(u0);

cbuffer GenLfData : register(b0) {
  uint cb_mi_stride;
  uint cb_mi_addr_base;
  uint cb_delta_q_info_delta_lf_present_flag;
  int cb_delta_q_info_delta_lf_multi;
  int cb_lf_mode_ref_delta_enabled;
  // Reserved here to keep the legacy constant and stage-dump layout. Rust entries are always 4 bytes.
  uint cb_mi_grid_entry;
  // Clamp speculative loads to the allocation, including at the padded chroma edge.
  uint cb_mi_grid_last;
  int reserved;
  int2 cb_lf_filter_level;
  int cb_lf_filter_level_u;
  int cb_lf_filter_level_v;
  int4 cb_lf_mode_deltas[2];
  int4 cb_lf_ref_deltas[8];
  int4 cb_seg_features[8];
  int4 cb_seg_data[64];
  int4 cb_lfi_n_lvl[3 * 8 * 8 * 2];
};

cbuffer GenLfSRT : register(b1) {
  uint cb_wicount;
  uint cb_mi_cols;
  uint cb_mi_rows;
  uint cb_plane;
  uint cb_dst_offset;
  uint cb_dst_stride;
};

uint get_mi_address(uint index) {
  return mi_grid.Load(min(index, cb_mi_grid_last) * 4);
}

int get_filter_level(int plane, int ref0, int mode, int segment_id, uint intra_params, uint delta) {
  segment_id = (segment_id >> 24) & 7;
  const int mode_lut = (0x017f6000 >> (mode & 255)) & 1;
  if (cb_delta_q_info_delta_lf_present_flag) {
    const int lut_val = plane + (LfHorizontal || plane > 0);
    int delta_lf;
    if (cb_delta_q_info_delta_lf_multi) {
      delta_lf = (int)((delta >> (8 * lut_val)) << 24) >> 24;
    } else {
      delta_lf = (int)(intra_params << 24) >> 24;
    }

    int base_level;
    if (plane == 0)
      base_level = LfHorizontal ? cb_lf_filter_level.y : cb_lf_filter_level.x;
    else if (plane == 1)
      base_level = cb_lf_filter_level_u;
    else
      base_level = cb_lf_filter_level_v;

    int lvl_seg = clamp(delta_lf + base_level, 0, 63);

    const int seg_lf_feature_id = lut_val + 1;
    if (cb_seg_features[segment_id].y & (1 << seg_lf_feature_id)) {
      const int d = cb_seg_data[segment_id * 8 + seg_lf_feature_id].x;
      lvl_seg = clamp(lvl_seg + d, 0, 63);
    }

    if (cb_lf_mode_ref_delta_enabled) {
      const int scale = 1 << (lvl_seg >> 5);
      lvl_seg += cb_lf_ref_deltas[ref0].x * scale;
      if (ref0 > 0) lvl_seg += cb_lf_mode_deltas[mode_lut].x * scale;
      lvl_seg = clamp(lvl_seg, 0, 63);
    }
    return lvl_seg;
  } else {
    int4 levels = cb_lfi_n_lvl[2 * 8 * 8 * plane + 2 * 8 * segment_id + 2 * ref0 + mode_lut];
    return LfHorizontal ? levels.y : levels.x;
  }
}

int get_tx_step(int plane, int col, int row, int bw_log, int bh_log, bool split_tx, uint tx_info, uint mi_index) {
  if (cb_seg_features[(tx_info >> 24) & 7].x) return 1;  // Lossless segment.
  if (plane != 0) return 1 << clamp((LfHorizontal ? bh_log : bw_log) - 1, 0, 3);

  uint tx_size = tx_info;
  if (split_tx) {
    const int blk_row = row & ((1 << bh_log) - 1);
    const int blk_col = col & ((1 << bw_log) - 1);
    const int bw_log1 = min(4, bw_log);
    const int bh_log1 = min(4, bh_log);
    const int tx_w_log = max(0, bw_log1 - (bw_log1 >= bh_log1));
    const int tx_h_log = max(0, bh_log1 - (bh_log1 >= bw_log1));
    const int idx = ((blk_row >> tx_h_log) << (bw_log - tx_w_log)) + (blk_col >> tx_w_log);
    tx_size = mi_pool[mi_index].inter_tx_size[idx >> 2] >> ((idx & 3) << 3);
  }
  const int tx_size_wide[] = {1, 2, 4, 8, 16, 1, 2, 2, 4, 4, 8, 8, 16, 1, 4, 2, 8, 4, 16};
  const int tx_size_high[] = {1, 2, 4, 8, 16, 2, 1, 4, 2, 8, 4, 16, 8, 4, 1, 8, 2, 16, 4};
  return LfHorizontal ? tx_size_high[tx_size & 255] : tx_size_wide[tx_size & 255];
}

struct EdgeMi {
  int step;
  int level;
  uint pred_mask;
  bool skipped;
};

EdgeMi read_edge_mi(uint address, int plane, int col, int row) {
  const int mi_size_wide_log2[] = {0, 0, 1, 1, 1, 2, 2, 2, 3, 3, 3, 4, 4, 4, 5, 5, 0, 2, 1, 3, 2, 4};
  const int mi_size_high_log2[] = {0, 1, 0, 1, 2, 1, 2, 3, 2, 3, 4, 3, 4, 5, 4, 5, 2, 0, 3, 1, 4, 2};
  const uint mi_index = (address - cb_mi_addr_base) / ModeInfoSize;
  const uint tx_info = mi_pool[mi_index].tx_info;
  const uint block_type = mi_pool[mi_index].block_type;
  const int bsize = block_type & 255;
  const int bw_log = mi_size_wide_log2[bsize];
  const int bh_log = mi_size_high_log2[bsize];
  const int ref0 = ((int)block_type << 8) >> 24;
  const bool inter = ref0 > 0 || (mi_pool[mi_index].intra_mode_flags & 0x100) != 0;
  const bool skip = (tx_info & 0xff00) != 0;

  EdgeMi mi;
  mi.step = get_tx_step(plane, col, row, bw_log, bh_log, inter && !skip, tx_info, mi_index);
  mi.level = get_filter_level(plane, ref0, mi_pool[mi_index].modes, tx_info,
                              mi_pool[mi_index].intra_params, mi_pool[mi_index].delta_lf);
  mi.pred_mask = (1 << max((LfHorizontal ? bh_log : bw_log) - (plane > 0), 0)) - 1;
  mi.skipped = skip && inter;
  return mi;
}

[numthreads(64, 1, 1)]
void main(uint3 thread : SV_DispatchThreadID) {
  if (thread.x >= cb_wicount) return;

  const int plane = cb_plane;
  const int subsampling = plane > 0;
  const int count_x = LfHorizontal ? cb_mi_cols : (cb_mi_cols + 15) >> 4;
  const int block_x = thread.x % count_x;
  const int block_y = thread.x / count_x;
  const int mi_col = LfHorizontal ? block_x : block_x * 16;
  const int mi_row = LfHorizontal ? block_y * 16 : block_y;
  const int start = LfHorizontal ? mi_row : mi_col;
  const int extent = LfHorizontal ? cb_mi_rows : cb_mi_cols;
  const int end = clamp(extent - start, 0, 16);
  const int dst_addr = cb_dst_offset + block_x * 32 + cb_dst_stride * mi_row;
  dst_buf.Store4(dst_addr, uint4(0, 0, 0, 0));
  dst_buf.Store4(dst_addr + 16, uint4(0, 0, 0, 0));

  const int unit = 1 << subsampling;
  const int grid_step = LfHorizontal ? cb_mi_stride * unit : unit;
  const int col0 = (mi_col << subsampling) | subsampling;
  const int row0 = (mi_row << subsampling) | subsampling;
  const uint grid_start = col0 + row0 * cb_mi_stride;
  EdgeMi preceding = {0, 0, 0, false};
  uint packed = 0;

  for (int pos = 0; pos < end;) {
    const uint index = grid_start + pos * grid_step;
    const int col = LfHorizontal ? col0 : col0 + pos * unit;
    const int row = LfHorizontal ? row0 + pos * unit : row0;
    const int coord = start + pos;

    const uint address = get_mi_address(index);
    if (address == 0) break;

    // If the last step was one MI, the decoded record is exactly the one on the near side
    // of this edge. For larger steps a split transform may change within the same mode record,
    // so read the MI at the actual coordinate and use its own transform index.
    const bool adjacent = preceding.step == 1;
    uint previous_address = 0;
    if (coord > 0 && !adjacent) {
      previous_address = get_mi_address(index - grid_step);
      if (previous_address == 0) break;
    }

    EdgeMi current = read_edge_mi(address, plane, col, row);
    if (coord & (current.step - 1)) break;
    uint edge = current.step << 8;
    if (coord > 0) {
      EdgeMi previous = preceding;
      if (!adjacent) {
        const int prev_col = LfHorizontal ? col : col - unit;
        const int prev_row = LfHorizontal ? row - unit : row;
        previous = read_edge_mi(previous_address, plane, prev_col, prev_row);
      }

      const bool pred_edge = (coord & current.pred_mask) == 0;
      if ((current.level || previous.level) && (!current.skipped || !previous.skipped || pred_edge)) {
        const int step = min(current.step, previous.step);
        const uint type = step == 1 ? 0u : plane != 0 ? 1u : step == 2 ? 2u : 3u;
        const int level = current.level ? current.level : previous.level;
        edge |= level | (type << 6);
      }
    }

    // Every transform step is a power of two and aligned to its size. An odd position can
    // only follow the even position of the same word, so packed holds its low half.
    packed = (pos & 1) ? packed | (edge << 16) : edge;
    dst_buf.Store(dst_addr + (pos & ~1) * 2, packed);
    preceding = current;
    pos += current.step;
  }
}
