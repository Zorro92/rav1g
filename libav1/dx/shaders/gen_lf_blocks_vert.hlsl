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
  int3 reserved;
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

int get_mi_index(ByteAddressBuffer grid, int index, uint base) {
  uint2 addr = grid.Load2(index * 8);
  if (addr.x == 0 && addr.y == 0)
    return -1;
  else {
    addr.x -= base;
    return addr.x / ModeInfoSize;
  }
}

int get_filter_level_hor(int plane, int ref0, int mode, int segment_id, int delta_from_base, int delta[4]) {
  segment_id = (segment_id >> 24) & 7;
  const int mode_lut = (0x017f6000 >> (mode & 255)) & 1;
  if (cb_delta_q_info_delta_lf_present_flag) {
    const int lut_val = plane + (plane > 0);  // plane + 1 for DIR = 1;
    int delta_lf;
    if (cb_delta_q_info_delta_lf_multi) {
      delta_lf = delta[lut_val];
    } else {
      delta_lf = delta_from_base;
    }

    int base_level;
    if (plane == 0)
      base_level = cb_lf_filter_level.x;
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
    return cb_lfi_n_lvl[2 * 8 * 8 * plane + 2 * 8 * segment_id + 2 * ref0 + mode_lut].x;
  }
}

int get_tx_size_hor(int plane, int col, int row, int bw_log, int bh_log, int is_inter, int skip, uint tx_info,
                    uint inter_tx_size[4]) {
  int txstep = 1;
  if (!cb_seg_features[(tx_info >> 24) & 7].x)  //! is_lossless?
  {
    int tx_size = 0;
    if (plane == 0) {
      if (is_inter && !skip) {
        const int blk_row = row & ((1 << bh_log) - 1);
        const int blk_col = col & ((1 << bw_log) - 1);
        int bw_log1 = min(4, bw_log);
        int bh_log1 = min(4, bh_log);
        int tx_w_log = max(0, bw_log1 - (bw_log1 >= bh_log1));
        int tx_h_log = max(0, bh_log1 - (bh_log1 >= bw_log1));
        const int idx = ((blk_row >> tx_h_log) << (bw_log - tx_w_log)) + (blk_col >> tx_w_log);
        tx_size = inter_tx_size[idx >> 2] >> ((idx & 3) << 3);
      } else {
        tx_size = tx_info;
      }
      tx_size &= 255;
      // tx_size = clamp(tx_size, 0, 18);
      const int tx_size_wide[] = {1, 2, 4, 8, 16, 1, 2, 2, 4, 4, 8, 8, 16, 1, 4, 2, 8, 4, 16};
      txstep = tx_size_wide[tx_size];
    } else {
      txstep = 1 << clamp(bw_log - 1, 0, 3);
    }
  }
  return txstep;
}

[numthreads(64, 1, 1)] void main(uint3 thread
                                 : SV_DispatchThreadID) {
  if (thread.x >= cb_wicount) return;

  const int mi_size_wide_log2[] = {0, 0, 1, 1, 1, 2, 2, 2, 3, 3, 3, 4, 4, 4, 5, 5, 0, 2, 1, 3, 2, 4};
  const int mi_size_high_log2[] = {0, 1, 0, 1, 2, 1, 2, 3, 2, 3, 4, 3, 4, 5, 4, 5, 2, 0, 3, 1, 4, 2};

  const int plane = cb_plane;
  const int subsampling = plane > 0;
  const int mi_cols = cb_mi_cols;

  const int count_x = (mi_cols + 15) >> 4;
  const int mi_col = (thread.x % count_x) * 16;
  const int mi_row = thread.x / count_x;

  int c_end = clamp(mi_cols - mi_col, 0, 16);
  const int dst_addr = cb_dst_offset + mi_col * 2 + cb_dst_stride * mi_row;
  dst_buf.Store4(dst_addr, uint4(0, 0, 0, 0));
  dst_buf.Store4(dst_addr + 16, uint4(0, 0, 0, 0));

  int err = 0;
  int c = 0;
  uint edges = 0;
  for (; c < c_end;) {
    const int col = ((mi_col + c) << subsampling) | subsampling;
    const int row = (mi_row << subsampling) | subsampling;

    const int mi0 = col + row * cb_mi_stride;
    const int mi_idx = get_mi_index(mi_grid, mi0, cb_mi_addr_base);

    int filter_length = 0;
    int filter_level = 0;

    if (mi_idx == -1) {
      err = 1;
      break;
    }
    uint tx_info = mi_pool[mi_idx].tx_info;
    uint blk_type = mi_pool[mi_idx].block_type;

    const int bsize = blk_type & 255;
    const int bw_log = mi_size_wide_log2[bsize];
    const int bh_log = mi_size_high_log2[bsize];
    const int skip = (tx_info & 0xff00) != 0;
    const int ref0 = ((int)blk_type << 8) >> 24;
    const int is_inter = ref0 > 0 || (mi_pool[mi_idx].intra_mode_flags & 0x100) != 0;
    const int ts =
        get_tx_size_hor(plane, col, row, bw_log, bh_log, is_inter, skip, tx_info, mi_pool[mi_idx].inter_tx_size);

    if ((mi_col + c) & (ts - 1)) {
      err = 2;
      break;
    }
    const int curr_level = get_filter_level_hor(plane, ref0, mi_pool[mi_idx].modes, tx_info,
                                                mi_pool[mi_idx].delta_lf_from_base, mi_pool[mi_idx].delta_lf);
    const int curr_skipped = skip && is_inter;
    filter_level = curr_level;
    if ((mi_col + c) > 0) {
      const int mi1 = mi0 - (1 << subsampling);
      const int mi1_idx = get_mi_index(mi_grid, mi1, cb_mi_addr_base);
      if (mi1_idx == -1) {
        err = 3;
        break;
      }
      uint pv_tx_info = mi_pool[mi1_idx].tx_info;
      uint pv_blk_type = mi_pool[mi1_idx].block_type;
      const int pv_bsize = pv_blk_type & 255;
      const int pv_bw_log = mi_size_wide_log2[pv_bsize];
      const int pv_bh_log = mi_size_high_log2[pv_bsize];
      const int pv_skip = (pv_tx_info & 0xff00) != 0;
      const int pv_ref0 = ((int)pv_blk_type << 8) >> 24;
      const int pv_is_inter = pv_ref0 > 0 || (mi_pool[mi1_idx].intra_mode_flags & 0x100) != 0;
      const int pv_ts = get_tx_size_hor(plane, col - (1 << subsampling), row, pv_bw_log, pv_bh_log, pv_is_inter,
                                        pv_skip, pv_tx_info, mi_pool[mi1_idx].inter_tx_size);
      const int pv_lvl = get_filter_level_hor(plane, pv_ref0, mi_pool[mi1_idx].modes, pv_tx_info,
                                              mi_pool[mi1_idx].delta_lf_from_base, mi_pool[mi1_idx].delta_lf);

      const int pu_edge = ((mi_col + c) & ((1 << max(bw_log - subsampling, 0)) - 1)) == 0;

      if ((curr_level || pv_lvl) && (!(pv_skip && pv_is_inter) || !curr_skipped || pu_edge)) {
        const int min_ts = min(ts, pv_ts);
        if (min_ts <= 1) {
          filter_length = 1;  // 4;
        } else if (min_ts == 2) {
          filter_length = 3 - subsampling;
        } else {
          filter_length = 4;
          if (plane != 0) {
            filter_length = 2;  // 6;
          }
        }
        filter_level = (curr_level) ? (curr_level) : (pv_lvl);
      }
    }

    uint edge = 0;
    if (filter_length)
      edge = filter_level | ((filter_length - 1) << 6) | (ts << 8);
    else
      edge = ts << 8;

    if (c & 1) edge = edges | (edge << 16);
    dst_buf.Store(dst_addr + (c & ~1) * 2, edge);
    edges = edge;
    c += ts;
  }
}
