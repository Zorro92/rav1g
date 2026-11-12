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

#include "dx/types.h"
#include "dx/av1_core.h"
#include "dx/av1_memory.h"
#include "dx/av1_compute.h"

#include "av1\common\filter.h"
#include "av1/common/reconinter.h"
#include "av1/common/reconintra.h"
#include "av1/common/warped_motion.h"
#include "aom_dsp/intrapred_common.h"

enum {
  NeedAboveLut = 0x3f7f,      // 11 1111 0111 1?11
  NeedLeftLut = 0x3Ef7,       // 11 1110 1111 01?1
  NeedRightLut = 0x010A,      // 0 0001 0000 10?0
  NeedBotLut = 0x0084,        // 0 0000 1000 0?00
  NeedAboveLeftLut = 0x11ff,  // 1 0001 1111 111?  - set to 1 for DC mode to workaround Filter mode
};

int get_relative_dist(const OrderHintInfo *oh, int a, int b) {
  if (!oh->enable_order_hint) return 0;

  const int bits = oh->order_hint_bits_minus_1 + 1;

  assert(bits >= 1);
  assert(a >= 0 && a < (1 << bits));
  assert(b >= 0 && b < (1 << bits));

  int diff = a - b;
  const int m = 1 << (bits - 1);
  diff = (diff & (m - 1)) - (diff & m);
  return diff;
}

void av1_mi_push_block(AV1Decoder *pbi, AV1_COMMON *cm, MACROBLOCKD *xd) {
  Av1Core *dec = pbi->gpu_decoder;
  av1_frame_thread_data *td = dec->curr_frame_data;
  av1_tile_data *tile = xd->tile_data;
  unsigned int *index_arr = tile->gen_indexes;

  MB_MODE_INFO *mi = xd->mi[0];
  mi->index_base = tile->gen_index_ptr;
  int bsize = mi->sb_type;
  const int bw_log = mi_size_wide_log2[bsize];
  const int bh_log = mi_size_high_log2[bsize];
  const int bw = 1 << bw_log;
  const int bh = 1 << bh_log;
  const int bw_log_uv = AOMMAX(0, bw_log - 1);
  const int bh_log_uv = AOMMAX(0, bh_log - 1);

  const int mi_row = mi->mi_row;
  const int mi_col = mi->mi_col;

  const int is_chroma_ref = ((mi_row & 1) == 0 && (bh & 1) == 1) || ((mi_col & 1) == 0 && (bw & 1) == 1);

  const int is_inter = mi->ref_frame[0] > INTRA_FRAME;
  const int is_inter_intra = is_interintra_pred(mi);
  const int is_compound = mi->ref_frame[1] > INTRA_FRAME;
  const int is_obmc = is_inter && mi->motion_mode == OBMC_CAUSAL && (xd->up_available || xd->left_available);
  if (is_inter) {
    tile->have_inter = 1;
    int is_global_warp[2] = {0, 0};
    if (xd->cur_frame_force_integer_mv == 0 && bsize >= BLOCK_8X8 &&
        (mi->mode == GLOBALMV || mi->mode == GLOBAL_GLOBALMV)) {
      for (int ref = 0; ref < 1 + is_compound; ++ref) {
        // As gen_pred_blocks.hlsl decides it from GenBlockData::global_warp: both count the block into the
        // same list.
        is_global_warp[ref] = td->gm_warp[mi->ref_frame[ref]];
      }
    }
    const int is_local_warp =
        xd->cur_frame_force_integer_mv == 0 && mi->motion_mode == WARPED_CAUSAL && !mi->wm_params.invalid;
    const int is_luma_warp = is_inter && (is_local_warp || is_global_warp[0]) && bw_log >= 1 && bh_log >= 1;
    const int is_chroma_warp = is_luma_warp && bw_log >= 2 && bh_log >= 2;

    // Y:
    const int block_size_id_y = InterBlockSizeIndexLUT[bw_log][bh_log];
    if (is_compound) {
      const int is_warp_compound = is_global_warp[0] || is_global_warp[1];
      int type = is_warp_compound ? CompoundGlobalWarp
                                  : mi->interinter_comp.type == COMPOUND_WEDGE
                                        ? CompoundMasked
                                        : mi->interinter_comp.type == COMPOUND_DIFFWTD ? CompoundDiff : CompoundAvrg;
      int type_index = (type - 1) * InterTypes::InterSizesAllCommon + block_size_id_y;
      index_arr[tile->gen_index_ptr++] = tile->gen_block_map[type_index]++;
    } else {
      if (is_luma_warp) {
        index_arr[tile->gen_index_ptr++] = tile->gen_block_map_wrp[block_size_id_y]++;
      } else {
        index_arr[tile->gen_index_ptr++] = tile->gen_block_map[block_size_id_y]++;
      }
    }

    // UV:
    if (!is_chroma_ref) {
      const int plane_bw = AOMMAX(0, bw_log - 1);
      const int plane_bh = AOMMAX(0, bh_log - 1);
      const int block_size_id_uv = InterBlockSizeIndexLUT[plane_bw][plane_bh];
      int sub8x8 = bw_log == 0 || bh_log == 0;
      if (sub8x8) {
        int dy = bh_log == 0 ? -1 : 0;
        int dx = bw_log == 0 ? -1 : 0;
        const MB_MODE_INFO *prev_mi[] = {
            xd->mi[dx + dy * xd->mi_stride],
            xd->mi[dx],
            xd->mi[dy * xd->mi_stride],
        };
        for (int i = 0; i < 3; ++i) {
          sub8x8 &= prev_mi[i]->ref_frame[0] > INTRA_FRAME;
        }
      }

      if (sub8x8) {
        const int brows = bh_log == 2 ? 4 : 2;
        const int bcols = bw_log == 2 ? 4 : 2;
        for (int row = 0; row < brows; ++row) {
          int mi_index = (bh_log == 0 ? row - 1 : 0) * xd->mi_stride;
          MB_MODE_INFO *sub_mi = xd->mi[mi_index];

          if (bw_log == 0) {
            MB_MODE_INFO *sub_mi0 = xd->mi[mi_index - 1];
            const int is_compound1 = sub_mi->ref_frame[1] > INTRA_FRAME;
            const int is_compound0 = sub_mi0->ref_frame[1] > INTRA_FRAME;
            int type_index0 = Inter2x2ArrOffset + (is_compound0 << static_cast<int>(is_compound0 != is_compound1));
            int type_index1 = Inter2x2ArrOffset + (is_compound1 << static_cast<int>(is_compound0 != is_compound1));
            index_arr[tile->gen_index_ptr++] = tile->gen_block_map[type_index0];
            if (type_index0 != type_index1) index_arr[tile->gen_index_ptr++] = tile->gen_block_map[type_index1];
            tile->gen_block_map[type_index0] += 2;
            tile->gen_block_map[type_index1] += 2;
          } else {
            const int type_index = Inter2x2ArrOffset + is_compound;
            index_arr[tile->gen_index_ptr++] = tile->gen_block_map[type_index];
            tile->gen_block_map[type_index] += 2 * bcols;
          }
        }
      } else {
        if (is_compound) {
          const int is_warp_compound = (is_global_warp[0] || is_global_warp[1]) && bw_log >= 2 && bh_log >= 2;
          int type =
              is_warp_compound
                  ? (mi->interinter_comp.type == COMPOUND_DIFFWTD ? CompoundDiffUvGlobalWarp : CompoundGlobalWarp)
                  : mi->interinter_comp.type == COMPOUND_WEDGE
                        ? CompoundMasked
                        : mi->interinter_comp.type == COMPOUND_DIFFWTD ? CompoundDiffUv : CompoundAvrg;
          int type_index = (type - 1) * InterTypes::InterSizesAllCommon + block_size_id_uv;
          index_arr[tile->gen_index_ptr++] = tile->gen_block_map[type_index];
          tile->gen_block_map[type_index] += 2;
        } else {
          if (is_chroma_warp) {
            index_arr[tile->gen_index_ptr++] = tile->gen_block_map_wrp[block_size_id_uv];
            tile->gen_block_map_wrp[block_size_id_uv] += 2;
          } else {
            index_arr[tile->gen_index_ptr++] = tile->gen_block_map[block_size_id_uv];
            tile->gen_block_map[block_size_id_uv] += 2;
          }
        }
      }
    }

    if (is_obmc) {
      int count = 0;
      if (xd->up_available) {
        const int x_mis = AOMMIN(bw, cm->mi_cols - mi_col);
        int h = bh_log > 4 ? 3 : (bh_log - 1);
        int huv = h == 0 ? 0 : h - 1;
        int obmc_chroma = bsize > BLOCK_16X8 && bsize != BLOCK_4X16 && bsize != BLOCK_16X4;
        for (int col = 0; col < x_mis && count < max_neighbor_obmc[bw_log];) {
          MB_MODE_INFO **above = xd->mi - xd->mi_stride + col;
          int w = mi_size_wide_log2[above[0]->sb_type];
          if (w == 0) {
            w = 1;
            ++above;
          }
          if (w > bw_log) w = bw_log;
          if (above[0]->ref_frame[0] > INTRA_FRAME) {
            count += 1 + (w == 5);
            const int type_index_y =
                (ObmcAbove - 1) * InterTypes::InterSizesAllCommon + ((w << 2) | h);  // InterBlockSizeIndexLUT[w][h];
            index_arr[tile->gen_index_ptr++] = tile->gen_block_map[type_index_y]++;
            if (obmc_chroma) {
              const int type_index_uv = (ObmcAbove - 1) * InterTypes::InterSizesAllCommon + (((w - 1) << 2) | huv);
              index_arr[tile->gen_index_ptr++] = tile->gen_block_map[type_index_uv];
              tile->gen_block_map[type_index_uv] += 2;
            }
          }

          assert(above[0]->use_intrabc == 0);
          col += 1 << w;
        }
      }
      count = 0;
      if (xd->left_available) {
        const int y_mis = AOMMIN(bh, cm->mi_rows - mi_row);
        int w = bw_log > 4 ? 3 : (bw_log - 1);
        int wuv = w == 0 ? 0 : w - 1;
        for (int row = 0; row < y_mis && count < max_neighbor_obmc[bh_log];) {
          MB_MODE_INFO **left = xd->mi - 1 + xd->mi_stride * row;
          int h = mi_size_high_log2[left[0]->sb_type];
          if (h == 0) {
            h = 1;
            left += xd->mi_stride;
          }
          if (h > bh_log) h = bh_log;

          if (left[0]->ref_frame[0] > INTRA_FRAME) {
            count += 1 + (h == 5);

            const int type_index_y = (ObmcLeft - 1) * InterTypes::InterSizesAllCommon + ((h << 2) | w);
            const int type_index_uv = (ObmcLeft - 1) * InterTypes::InterSizesAllCommon + (((h - 1) << 2) | wuv);
            index_arr[tile->gen_index_ptr++] = tile->gen_block_map[type_index_y]++;
            index_arr[tile->gen_index_ptr++] = tile->gen_block_map[type_index_uv];
            tile->gen_block_map[type_index_uv] += 2;
          }
          assert(left[0]->use_intrabc == 0);
          row += 1 << h;
        }
      }
    }
  }

  const int y_use_palette = mi->palette_mode_info.palette_size[0] > 0;
  const int uv_use_palette = mi->palette_mode_info.palette_size[1] > 0;
  if (!is_inter || is_inter_intra) {
    const int interintra_mode = mi->interintra_mode;
    const int is_intrabc = mi->use_intrabc;
    const BLOCK_SIZE bsize_uv = scale_chroma_bsize((BLOCK_SIZE)bsize, 1, 1);
    TX_SIZE tx_sizes[2];
    tx_sizes[0] = av1_get_tx_size(0, xd);
    tx_sizes[1] = av1_get_tx_size(1, xd);
    const int txw = is_intrabc ? AOMMIN(bw_log, 4) : is_inter_intra ? bw_log : (tx_size_wide_log2[tx_sizes[0]] - 2);
    const int txh = is_intrabc ? AOMMIN(bh_log, 4) : is_inter_intra ? bh_log : (tx_size_high_log2[tx_sizes[0]] - 2);
    const int max_cnt_x = (cm->mi_cols - mi_col + (1 << txw) - 1) >> txw;
    const int max_cnt_y = (cm->mi_rows - mi_row + (1 << txh) - 1) >> txh;
    const int unit_x_log = bw_log == 5 && !is_intrabc;
    const int unit_y_log = bh_log == 5 && !is_intrabc;
    int cnt_y = 1 << (bh_log - txh);
    int cnt_x = 1 << (bw_log - txw);
    if (!y_use_palette) {
      int iter_grid_stride = td->iter_grid_stride;
      int *iter_grid = td->gen_intra_iter_y + mi_col + mi_row * iter_grid_stride;
      const int mode = is_inter_intra ? interintra_to_intra_mode[interintra_mode] : mi->mode;

      int need_above = (NeedAboveLut >> mode) & 1;
      int need_left = (NeedLeftLut >> mode) & 1;
      const int need_right = (NeedRightLut >> mode) & 1;
      const int need_bot = (NeedBotLut >> mode) & 1;
      int need_aboveleft = (NeedAboveLeftLut >> mode) & 1;
      const int use_filter = mi->filter_intra_mode_info.use_filter_intra;

      const int type_size = txw + txh;
      const int type_idx_base = use_filter ? IntraBlockOffset : (IntraBlockOffset - 1 - type_size);

      if (is_intrabc) {
        need_above = 0;
        need_left = 0;
        need_aboveleft = 0;
      }

      cnt_x >>= unit_x_log;
      cnt_y >>= unit_y_log;
      for (int unit_y = 0; unit_y <= unit_y_log; ++unit_y) {
        for (int unit_x = 0; unit_x <= unit_x_log; ++unit_x) {
          const int x_start = unit_x * cnt_x;
          const int x_end = AOMMIN(x_start + cnt_x, max_cnt_x);
          const int y_start = unit_y * cnt_y;
          const int y_end = AOMMIN(y_start + cnt_y, max_cnt_y);
          for (int y = y_start; y < y_end; ++y) {
            for (int x = x_start; x < x_end; ++x) {
              const int col_off = x << txw;
              const int row_off = y << txh;
              const int subblk_w = 1 << txw;
              const int subblk_h = 1 << txh;
              const int have_top = y || xd->up_available;
              const int have_left = x || xd->left_available;

              int above_available = have_top;
              int have_top_right = 0;
              if (need_above) {
                const int xr = (xd->mb_to_right_edge >> 3) + ((4 << bw_log) - (col_off << 2) - (4 << txw));
                if (need_right) {
                  const int right_available = mi_col + (col_off + subblk_w) < xd->tile.mi_col_end;
                  have_top_right = has_top_right(cm, (BLOCK_SIZE)bsize, mi_row, mi_col, have_top, right_available,
                                                 mi->partition, tx_sizes[0], row_off, col_off, 0, 0);
                }
                above_available = (have_top ? AOMMIN(subblk_w, subblk_w + (xr >> 2)) : 0) +
                                  (have_top_right ? AOMMIN(subblk_w, xr >> 2) : 0);
              }

              int left_available = have_left;
              int have_bottom_left = 0;
              if (need_left) {
                const int yd = (xd->mb_to_bottom_edge >> 3) + ((4 << bh_log) - (row_off << 2) - (4 << txh));
                if (need_bot) {
                  const int bottom_available = (yd > 0) && ((mi_row + row_off + subblk_h) < xd->tile.mi_row_end);
                  have_bottom_left = has_bottom_left(cm, (BLOCK_SIZE)bsize, mi_row, mi_col, bottom_available, have_left,
                                                     mi->partition, tx_sizes[0], row_off, col_off, 0, 0);
                }

                left_available = (have_left ? AOMMIN(subblk_h, subblk_h + (yd >> 2)) : 0) +
                                 (have_bottom_left ? AOMMIN(subblk_h, yd >> 2) : 0);
              }

              int iter = -1;
              int *iter_grid_blk = iter_grid + subblk_w * x + subblk_h * y * iter_grid_stride;
              const int above_left = have_top && have_left && need_aboveleft;
              if (td->intra_v2) {
                // -intrav2: no wavefront iteration. The block gets a step and a pass from the same neighbours
                // (intra_v2_sched.h) and counts as iteration 0; gen_pred_blocks.hlsl reads that from this entry.
                intra_v2_place(&td->intra_v2_sched, tile->gen_block_map_offset + IntraBlockOffset - IntraSizes, 0,
                               mi_col + col_off, mi_row + row_off, txw, txh, above_left, above_available,
                               left_available, 0, type_idx_base - (IntraBlockOffset - IntraSizes),
                               tile->gen_block_map[type_idx_base]);
                iter = 0;
                tile->gen_intra_iter_y = AOMMAX(iter, tile->gen_intra_iter_y);
                iter_grid_blk[iter_grid_stride + subblk_w] = iter;
              } else {
                for (int it = 1 - above_left; it <= above_available; ++it) {
                  iter = AOMMAX(iter, iter_grid_blk[it]);
                }
                for (int it = 1; it <= left_available; ++it) {
                  iter = AOMMAX(iter, iter_grid_blk[it * iter_grid_stride]);
                }
                ++iter;
                if (is_intrabc) iter = tile->gen_intra_iter_y + 1;
                iter = AOMMIN(iter, tile->gen_intra_max_iter);
                tile->gen_intra_iter_y = AOMMAX(iter, tile->gen_intra_iter_y);
                for (int it = 1; it <= subblk_h; ++it) {
                  iter_grid_blk[it * iter_grid_stride + subblk_w] = iter;
                }
                for (int it = 1; it <= subblk_w; ++it) {
                  iter_grid_blk[it + subblk_h * iter_grid_stride] = iter;
                }
              }

              if (iter > tile->gen_intra_iter_set) {
                memset(tile->gen_block_map + tile->gen_iter_clear_offset, 0, tile->gen_iter_clear_size);
                tile->gen_intra_iter_set = tile->gen_intra_max_iter;
              }

              const int type_index = iter * IntraTypeCount + type_idx_base;
              index_arr[tile->gen_index_ptr++] =
                  (tile->gen_block_map[type_index] << 2) | have_top_right | (have_bottom_left << 1);
              ++tile->gen_block_map[type_index];
            }
          }
        }
      }
    }

    if (!uv_use_palette && !is_chroma_ref) {
      const int mi_col_uv = mi_col >> 1;
      const int mi_row_uv = mi_row >> 1;
      const int iter_grid_stride_uv = td->iter_grid_stride_uv;
      int *iter_grid = td->gen_intra_iter_uv + mi_col_uv + mi_row_uv * iter_grid_stride_uv;
      const int mode = is_inter_intra ? interintra_to_intra_mode[interintra_mode] : mi->uv_mode;

      int need_above = (NeedAboveLut >> mode) & 1;
      int need_left = (NeedLeftLut >> mode) & 1;
      const int need_right = (NeedRightLut >> mode) & 1;
      const int need_bot = (NeedBotLut >> mode) & 1;
      int need_aboveleft = (NeedAboveLeftLut >> mode) & 1;

      const int txw = (is_intrabc || is_inter_intra) ? bw_log_uv : (tx_size_wide_log2[tx_sizes[1]] - 2);
      const int txh = (is_intrabc || is_inter_intra) ? bh_log_uv : (tx_size_high_log2[tx_sizes[1]] - 2);

      const int cnt_y = 1 << (bh_log_uv - txh - unit_y_log);
      const int cnt_x = 1 << (bw_log_uv - txw - unit_x_log);

      const int type_size = txw + txh;

      if (is_intrabc) {
        need_above = 0;
        need_left = 0;
        need_aboveleft = 0;
      }

      for (int unit_y = 0; unit_y <= unit_y_log; ++unit_y) {
        for (int unit_x = 0; unit_x <= unit_x_log; ++unit_x) {
          for (int suby = 0; suby < cnt_y; ++suby) {
            for (int subx = 0; subx < cnt_x; ++subx) {
              const int x = subx + unit_x * cnt_x;
              const int y = suby + unit_y * cnt_y;
              const int col_off = x << txw;
              const int row_off = y << txh;
              const int subblk_w = 1 << txw;
              const int subblk_h = 1 << txh;
              const int have_top = y || xd->chroma_up_available;
              const int have_left = x || xd->chroma_left_available;
              int above_available = have_top;
              int have_top_right = 0;
              if (need_above) {
                const int xr = (xd->mb_to_right_edge >> 4) + ((4 << bw_log_uv) - (col_off << 2) - (4 << txw));
                if (need_right) {
                  const int right_available = mi_col + ((col_off + subblk_w) << 1) < xd->tile.mi_col_end;
                  have_top_right = has_top_right(cm, bsize_uv, mi_row, mi_col, have_top, right_available, mi->partition,
                                                 tx_sizes[1], row_off, col_off, 1, 1);
                }
                above_available = (have_top ? AOMMIN(subblk_w, subblk_w + (xr >> 2)) : 0) +
                                  (have_top_right ? AOMMIN(subblk_w, xr >> 2) : 0);
              }

              int left_available = have_left;
              int have_bottom_left = 0;
              if (need_left) {
                const int yd = (xd->mb_to_bottom_edge >> 4) + ((4 << bh_log_uv) - (row_off << 2) - (4 << txh));
                if (need_bot) {
                  const int bottom_available =
                      (yd > 0) && ((mi_row + ((row_off + subblk_h) << 1)) < xd->tile.mi_row_end);
                  have_bottom_left = has_bottom_left(cm, bsize_uv, mi_row, mi_col, bottom_available, have_left,
                                                     mi->partition, tx_sizes[1], row_off, col_off, 1, 1);
                }

                left_available = (have_left ? AOMMIN(subblk_h, subblk_h + (yd >> 2)) : 0) +
                                 (have_bottom_left ? AOMMIN(subblk_h, yd >> 2) : 0);
              }

              int iter = -1;
              int *iter_grid_blk = iter_grid + subblk_w * x + subblk_h * y * iter_grid_stride_uv;
              const int above_left = need_aboveleft && have_top && have_left;
              if (td->intra_v2) {
                // -intrav2, as for luma. U and V share the place; CFL also waits for the luma under the block.
                intra_v2_place(&td->intra_v2_sched, tile->gen_block_map_offset + IntraBlockOffset - IntraSizes, 1,
                               mi_col_uv + col_off, mi_row_uv + row_off, txw, txh, above_left, above_available,
                               left_available, mode == UV_CFL_PRED, IntraSizes - 1 - type_size,
                               tile->gen_block_map[IntraBlockOffset - 1 - type_size]);
                iter = 0;
                tile->gen_intra_iter_uv = AOMMAX(iter, tile->gen_intra_iter_uv);
                iter_grid_blk[iter_grid_stride_uv + subblk_w] = iter;
              } else {
                for (int it = 1 - above_left; it <= above_available; ++it) {
                  iter = AOMMAX(iter, iter_grid_blk[it]);
                }
                for (int it = 1; it <= left_available; ++it) {
                  iter = AOMMAX(iter, iter_grid_blk[it * iter_grid_stride_uv]);
                }
                if (mode == UV_CFL_PRED) {
                  int iter_grid_stride = td->iter_grid_stride;
                  int *y_grid = td->gen_intra_iter_y + ((mi_col_uv + col_off) << 1) +
                                ((mi_row_uv + row_off) << 1) * iter_grid_stride;
                  for (int yit = 1; yit <= (subblk_h << 1); ++yit) {
                    for (int xit = 1; xit <= (subblk_w << 1); ++xit) {
                      iter = AOMMAX(iter, y_grid[xit + yit * iter_grid_stride]);
                    }
                  }
                }

                ++iter;
                if (is_intrabc) iter = tile->gen_intra_iter_uv + 1;
                iter = AOMMIN(iter, tile->gen_intra_max_iter);
                tile->gen_intra_iter_uv = AOMMAX(iter, tile->gen_intra_iter_uv);
                for (int it = 1; it <= subblk_h; ++it) {
                  iter_grid_blk[it * iter_grid_stride_uv + subblk_w] = iter;
                }
                for (int it = 1; it <= subblk_w; ++it) {
                  iter_grid_blk[it + subblk_h * iter_grid_stride_uv] = iter;
                }
              }

              if (iter > tile->gen_intra_iter_set) {
                memset(tile->gen_block_map + tile->gen_iter_clear_offset, 0, tile->gen_iter_clear_size);
                tile->gen_intra_iter_set = tile->gen_intra_max_iter;
              }

              const int type_index = iter * IntraTypeCount + IntraBlockOffset - 1 - type_size;
              index_arr[tile->gen_index_ptr++] =
                  (tile->gen_block_map[type_index] << 2) | have_top_right | (have_bottom_left << 1);
              tile->gen_block_map[type_index] += 2;
            }
          }
        }
      }
    }
  }

  if (mi->skip == 0) {
    if (y_use_palette || is_obmc) {
      const int type_index = ReconBlockOffset + bw_log + 6 * bh_log;
      index_arr[tile->gen_index_ptr++] = tile->gen_block_map[type_index]++;
    }
    if (uv_use_palette || (is_obmc && !is_chroma_ref)) {
      const int type_index = ReconBlockOffset + bw_log_uv + 6 * bh_log_uv;
      index_arr[tile->gen_index_ptr++] = tile->gen_block_map[type_index];
      tile->gen_block_map[type_index] += 2;
    }
  }
}

struct GenBlockData {
  int mi_cols;
  int mi_rows;
  int mi_stride;
  unsigned int mi_addr_base;
  int iter_grid_stride;
  int iter_grid_offset_uv;
  int iter_grid_stride_uv;
  int disable_edge_filter;
  int force_integet_mv;
  int reserved[3];
  int wedge_offsets[22][4];  //??
  int dist_wtd[8 * 8][4];
  int lossless_seg[8][4];

  int global_warp[8][4];
  struct {
    WarpedMotionParams params;
    int pad;
  } wm_params[8];
};

struct GenBlockSRT {
  int wi_count;
  int mi_offset;
  int mi_idx_base;
  int col_srart;
  int row_srart;
  int index_offset;
  int index_offset_warp;
};

void av1_prediction_gen_blocks(AV1Decoder *pbi, Av1Core *dec) {
  AV1_COMMON *cm = &pbi->common;
  MACROBLOCKD *const xd = &pbi->mb;
  auto td = dec->curr_frame_data;

  ComputeCommandBuffer *cb = &td->command_buffer;
  ConstantBufferObject cbo = cb->Alloc(sizeof(GenBlockData));
  GenBlockData *data = (GenBlockData *)cbo.host_ptr;
  data->mi_cols = cm->mi_cols;
  data->mi_rows = cm->mi_rows;
  data->mi_stride = cm->mi_stride;
  data->mi_addr_base = static_cast<unsigned int>(reinterpret_cast<uint64_t>(dec->mode_info_pool->host_ptr));
  data->iter_grid_stride = td->iter_grid_stride;
  data->iter_grid_offset_uv = td->iter_grid_offset_uv;
  data->iter_grid_stride_uv = td->iter_grid_stride_uv;
  data->disable_edge_filter = !cm->seq_params.enable_intra_edge_filter;
  data->force_integet_mv = cm->cur_frame_force_integer_mv;
  for (int bs = 0; bs < 22; ++bs) {
    data->wedge_offsets[bs][0] = dec->wedge_offsets[bs][0];
    data->wedge_offsets[bs][1] = dec->wedge_offsets[bs][1];
  }
  for (int i = 0; i < 8; ++i) data->lossless_seg[i][0] = xd->lossless[i];
  for (int i = 0; i < 7; ++i) data->global_warp[i][0] = td->gm_warp[i + 1];
  for (int r1 = 0; r1 < 7; ++r1) {
    for (int r0 = 0; r0 < 7; ++r0) {
      int fwd_offset = 8;
      int bck_offset = 8;
      const RefCntBuffer *const bck_buf = get_ref_frame_buf(cm, r0 + 1);
      const RefCntBuffer *const fwd_buf = get_ref_frame_buf(cm, r1 + 1);
      const int cur_frame_index = cm->cur_frame->order_hint;
      int bck_frame_index = 0, fwd_frame_index = 0;
      if (bck_buf != NULL) bck_frame_index = bck_buf->order_hint;
      if (fwd_buf != NULL) fwd_frame_index = fwd_buf->order_hint;

      int d0 = clamp(abs(get_relative_dist(&cm->seq_params.order_hint_info, fwd_frame_index, cur_frame_index)), 0,
                     MAX_FRAME_DISTANCE);
      int d1 = clamp(abs(get_relative_dist(&cm->seq_params.order_hint_info, cur_frame_index, bck_frame_index)), 0,
                     MAX_FRAME_DISTANCE);
      const int order = d0 <= d1;
      if (d0 == 0 || d1 == 0) {
        fwd_offset = quant_dist_lookup_table[0][3][order];
        bck_offset = quant_dist_lookup_table[0][3][1 - order];
      } else {
        int i;
        for (i = 0; i < 3; ++i) {
          int c0 = quant_dist_weight[i][order];
          int c1 = quant_dist_weight[i][!order];
          int d0_c0 = d0 * c0;
          int d1_c1 = d1 * c1;
          if ((d0 > d1 && d0_c0 < d1_c1) || (d0 <= d1 && d0_c0 > d1_c1)) break;
        }
        fwd_offset = quant_dist_lookup_table[0][i][order];
        bck_offset = quant_dist_lookup_table[0][i][1 - order];
      }
      data->dist_wtd[r0 + 8 * r1][0] = fwd_offset | (bck_offset << 4);
    }
  }
  for (int i = 0; i < 7; ++i)
    memcpy(&data->wm_params[i].params, &xd->global_motion[i + 1], sizeof(data->wm_params[0].params));

  const int tile_count = td->tile_count;
  av1_tile_data *tiles = td->tile_data;
  int itra_iters = -1;

  int main_tile = 0;
  for (int t = 0; t < tile_count; ++t) {
    int tile_iters = -1;
    tile_iters = AOMMAX(tile_iters, tiles[t].gen_intra_iter_y);
    tile_iters = AOMMAX(tile_iters, tiles[t].gen_intra_iter_uv);
    if (tile_iters > itra_iters) {
      itra_iters = tile_iters;
      main_tile = t;
    }
    tiles[t].gen_pred_map_max =
        InterTypes::InterCountsAll + ReconstructBlockSizes + (tile_iters + 1) * IntraTypeCount + 1;
  }

  if (main_tile) {
    av1_tile_data tile0 = tiles[0];
    tiles[0] = tiles[main_tile];
    tiles[main_tile] = tile0;
    if (td->sec_thread_data) {
      av1_tile_data *tile2 = td->sec_thread_data->tile_data;
      tile0 = tile2[0];
      tile2[0] = tile2[main_tile];
      tile2[main_tile] = tile0;
    }
  }

  td->intra_iters = itra_iters;
  const int pred_block_types = InterTypes::InterCountsAll + ReconstructBlockSizes + (itra_iters + 1) * IntraTypeCount;
  int offset = 0;
  for (int it = 0; it <= pred_block_types; ++it) {
    for (int t = 0; t < tile_count; ++t) {
      if (it < tiles[t].gen_pred_map_max) {
        int cnt = tiles[t].gen_block_map[it];
        tiles[t].gen_block_map[it] = offset;
        offset += cnt;
      }
    }
  }
  const int pred_block_count = offset;  // records of prediction_blocks the frame's lists take
  offset = 0;
  for (int it = 0; it <= InterTypes::InterSizesAllCommon; ++it) {
    for (int t = 0; t < tile_count; ++t) {
      int cnt = tiles[t].gen_block_map_wrp[it];
      tiles[t].gen_block_map_wrp[it] = offset;
      offset += cnt;
    }
  }
  const int pred_warp_count = offset;  // records of prediction_blocks_warp
  // -dumpstages: the mode info, where the first stage that reads it starts.
  av1_stage_dump_mode_info(dec, cm);
  Microsoft::WRL::ComPtr<ID3D12GraphicsCommandList> command_list = dec->compute.command_list;
  ComputeShader *shader = &dec->shader_lib->shader_gen_pred_blocks;
  command_list->SetComputeRootSignature(shader->signaturePtr.Get());
  command_list->SetPipelineState(shader->pso.Get());
  command_list->SetComputeRootShaderResourceView(0, dec->mode_info_pool->dev->GetGPUVirtualAddress());
  command_list->SetComputeRootShaderResourceView(1, td->gen_mi_block_indexes->dev->GetGPUVirtualAddress());
  command_list->SetComputeRootShaderResourceView(2, td->gen_block_map->dev->GetGPUVirtualAddress());
  command_list->SetComputeRootShaderResourceView(3, td->mode_info_grid->dev->GetGPUVirtualAddress());
  command_list->SetComputeRootShaderResourceView(4, td->gen_intra_inter_grid->dev->GetGPUVirtualAddress());
  command_list->SetComputeRootUnorderedAccessView(5, dec->prediction_blocks->dev->GetGPUVirtualAddress());
  command_list->SetComputeRootUnorderedAccessView(6, dec->prediction_blocks_warp->dev->GetGPUVirtualAddress());
  command_list->SetComputeRootConstantBufferView(7, cbo.dev_address);
  GenBlockSRT srt;
  for (int i = 0; i < tile_count; ++i) {
    srt.wi_count = tiles[i].mi_count;
    srt.mi_offset = tiles[i].mi_offset;
    srt.mi_idx_base = tiles[i].gen_index_base;
    srt.col_srart = tiles[i].mi_col_start;
    srt.row_srart = tiles[i].mi_row_start;
    srt.index_offset = tiles[i].gen_block_map_offset;
    srt.index_offset_warp = tiles[i].gen_block_warp_offset;
    command_list->SetComputeRoot32BitConstants(8, 7, &srt, 0);
    command_list->Dispatch((srt.wi_count + 63) >> 6, 1, 1);
  }

  if (td->sec_thread_data) {
    av1_tile_data *tiles2 = td->sec_thread_data->tile_data;
    for (int i = 0; i < tile_count; ++i) {
      srt.wi_count = tiles2[i].mi_count;
      srt.mi_offset = tiles2[i].mi_offset;
      if (srt.wi_count) {
        srt.mi_idx_base = tiles[i].gen_index_base;
        srt.col_srart = tiles[i].mi_col_start;
        srt.row_srart = tiles[i].mi_row_start;
        srt.index_offset = tiles[i].gen_block_map_offset;
        srt.index_offset_warp = tiles[i].gen_block_warp_offset;
        command_list->SetComputeRoot32BitConstants(8, 7, &srt, 0);
        command_list->Dispatch((srt.wi_count + 63) >> 6, 1, 1);
      }
    }
  }
  D3D12_RESOURCE_BARRIER barriers[] = {CD3DX12_RESOURCE_BARRIER::UAV(dec->prediction_blocks->dev),
                                       CD3DX12_RESOURCE_BARRIER::UAV(dec->prediction_blocks_warp->dev)};
  command_list->ResourceBarrier(2, barriers);
  PutPerfMarker(td, &td->perf_markers[1]);
  av1_perf_gpu_mark(dec, AV1_PERF_GPU_GEN_BLOCKS);
  // -dumpstages: the block records gen_pred_blocks has just written, as the prediction stages read them.
  if (av1_stage_dump_active(dec)) {
    enum { PredBlockSize = 16, PredWarpBlockSize = 48 };
    char layout[160];
    av1_stage_dump_region region = {dec->prediction_blocks->dev, 0, (size_t)pred_block_count * PredBlockSize, 1};
    if (region.size > dec->prediction_blocks->size) region.size = dec->prediction_blocks->size;
    snprintf(layout, sizeof(layout), "buffer=pred_blocks record=%d count=%d\n", (int)PredBlockSize,
             (int)(region.size / PredBlockSize));
    av1_stage_dump_data(dec, "pred_blocks", layout, &region, 1);
    region.src = dec->prediction_blocks_warp->dev;
    region.size = (size_t)pred_warp_count * PredWarpBlockSize;
    if (region.size > dec->prediction_blocks_warp->size) region.size = dec->prediction_blocks_warp->size;
    snprintf(layout, sizeof(layout), "buffer=pred_blocks_warp record=%d count=%d\n", (int)PredWarpBlockSize,
             (int)(region.size / PredWarpBlockSize));
    av1_stage_dump_data(dec, "pred_blocks_warp", layout, &region, 1);
  }
}

struct GlobalMotionWarp {
  int is_warp;
  int reserved;
  int mat[6];
  int alpha;
  int beta;
  int delta;
  int gamma;
};

typedef struct {
  PlaneInfo planes[3];
  RefPlaneInfo refplanes[3 * 7];
  int dims[2][4];
  int pixel_max[4];
  int kernels[8 * 16][8];
  ScaleFactor scale[8];
  int obmc_mask[(1 + 1 + 2 + 4 + 8) * 4];
  GlobalMotionWarp warp[7];
} PSSLInterData;

typedef struct {
  unsigned int wi_count;
  unsigned int pass_offset;
  unsigned int width_log2;
  unsigned int height_log2;
} PSSLInterSRT;

struct PSSLIntraData {
  PlaneInfo planes[3];
  int flags[4];
  int filter[5][8][8];
  int mode_params_lut[16][7][4];
  int sm_weight_arrays[128][4];
};

struct PSSLIntraSRT {
  int counts0[8];
  int wi_count;
  int pass_offset;
};

struct PSSLReconSRT {
  int wi_count;
  int pass_offset;
  int width_log2;
  int height_log2;
  int fb_base_offset;
  int r[5];
};

// Test player switch (-intraopt): the original intra path takes intra_main from the pipeline state built with
// FXC optimisation (intra_main_opt.hlsl / intra_main_hbd_opt.hlsl, which only include the original source)
// instead of the /Od build the project asks for. Off unless av1_set_intra_opt(1) was called.
static int g_intra_opt = 0;

extern "C" void av1_set_intra_opt(int enable) { g_intra_opt = enable; }

// Test flag -intraedgefix: intra_main.hlsl (8-bit) stops the taps of the above edge filter at filter_count,
// as intra_main_hbd.hlsl does, instead of at above_count. The output is the same (see the shader). Off unless
// av1_set_intra_edge_fix(1) was called.
static int g_intra_edge_fix = 0;

extern "C" void av1_set_intra_edge_fix(int enable) { g_intra_edge_fix = enable; }

void av1_prediction_run_all(Av1Core *dec, AV1_COMMON *cm, TileInfo *tile) {
  av1_frame_thread_data *td = dec->curr_frame_data;
  const int tile_count = td->tile_count;
  av1_tile_data *tiles = td->tile_data;

  // 1 Prepare command buffer and common data
  bitdepth_dependent_shaders *shaders = td->shaders;
  ComputeCommandBuffer *cb = &td->command_buffer;

  int do_inter = 0;
  for (int i = 0; i < tile_count; ++i) {
    do_inter |= tiles[i].have_inter;
  }

  Microsoft::WRL::ComPtr<ID3D12GraphicsCommandList> command_list = dec->compute.command_list;
  if (do_inter) {
    ConstantBufferObject cbo = cb->Alloc(sizeof(PSSLInterData));
    PSSLInterData *inter_data = (PSSLInterData *)cbo.host_ptr;
    memcpy(inter_data->planes, td->frame_buffer->planes, sizeof(inter_data->planes));
    memcpy(inter_data->scale, td->scale_factors, sizeof(inter_data->scale));
    for (int i = 0; i < 7; ++i) {
      for (int p = 0; p < 3; ++p) {
        inter_data->refplanes[i * 3 + p].offset = td->refs[i]->planes[p].offset;
        inter_data->refplanes[i * 3 + p].stride = td->refs[i]->planes[p].stride;
        inter_data->refplanes[i * 3 + p].width =
            p == 0 ? td->refs[i]->y_crop_width - 1 : td->refs[i]->uv_crop_width - 1;
        inter_data->refplanes[i * 3 + p].height =
            p == 0 ? td->refs[i]->y_crop_height - 1 : td->refs[i]->uv_crop_height - 1;
      }
    }
    inter_data->dims[0][0] = inter_data->refplanes[0].width;
    inter_data->dims[0][1] = inter_data->refplanes[0].height;
    inter_data->dims[1][0] = inter_data->refplanes[1].width;
    inter_data->dims[1][1] = inter_data->refplanes[1].height;

    memset(inter_data->kernels, 0, sizeof(inter_data->kernels));
    // add 4tap filters first:
    const int16_t *ker_arr[] = {
        (const int16_t *)av1_sub_pel_filters_4,       (const int16_t *)av1_sub_pel_filters_4smooth,
        (const int16_t *)av1_bilinear_filters,        (const int16_t *)av1_sub_pel_filters_8,
        (const int16_t *)av1_sub_pel_filters_8smooth, (const int16_t *)av1_sub_pel_filters_8sharp};
    for (int i = 0; i < 6; ++i)
      for (int j = 0; j < 16; ++j)
        for (int k = 0; k < 8; ++k) inter_data->kernels[i * 16 + j][k] = ker_arr[i][j * 8 + k];
    memcpy(inter_data->obmc_mask, obmc_mask, sizeof(obmc_mask));

    int enable_gm_warp = 0;
    for (int i = 0; i < 7; ++i) {
      WarpedMotionParams *wm = &cm->global_motion[i + 1];
      const int is_gmwarp = td->gm_warp[i + 1];  // not for a scaled reference: av1_setup_frame()
      enable_gm_warp |= is_gmwarp;
      inter_data->warp[i].is_warp = is_gmwarp;
      if (is_gmwarp) {
        inter_data->warp[i].mat[0] = wm->wmmat[0];
        inter_data->warp[i].mat[1] = wm->wmmat[1];
        inter_data->warp[i].mat[2] = wm->wmmat[2];
        inter_data->warp[i].mat[3] = wm->wmmat[3];
        inter_data->warp[i].mat[4] = wm->wmmat[4];
        inter_data->warp[i].mat[5] = wm->wmmat[5];
        inter_data->warp[i].alpha = wm->alpha;
        inter_data->warp[i].beta = wm->beta;
        inter_data->warp[i].delta = wm->delta;
        inter_data->warp[i].gamma = wm->gamma;
      }
    }

    // 2 Run independent blocks:
    ComputeShader *shaders_inter[InterTypes::Inter2x2 + 1];
    shaders_inter[InterTypes::Warp] = &shaders->inter_warp;
    const int wi_per_block = td->scale_enable ? 4 : 1;
    if (td->scale_enable) {
      shaders_inter[InterTypes::CasualInter] = &shaders->inter_scale;
      shaders_inter[InterTypes::CompoundAvrg] = &shaders->inter_scale_comp;
      shaders_inter[InterTypes::CompoundDiff] = &shaders->inter_scale_comp_diff_y;
      shaders_inter[InterTypes::CompoundMasked] = &shaders->inter_scale_comp_masked;
      shaders_inter[InterTypes::CompoundDiffUv] = &shaders->inter_scale_comp_diff_uv;
      shaders_inter[InterTypes::ObmcAbove] = &shaders->inter_scale_obmc_above;
      shaders_inter[InterTypes::ObmcLeft] = &shaders->inter_scale_obmc_left;
      shaders_inter[InterTypes::Inter2x2] = &shaders->inter_scale_2x2;
      shaders_inter[InterTypes::CompoundGlobalWarp] = &shaders->inter_warp_comp;
      shaders_inter[InterTypes::CompoundDiffUvGlobalWarp] = &shaders->inter_warp_comp;
    } else {
      shaders_inter[InterTypes::CasualInter] = &shaders->inter_base;
      shaders_inter[InterTypes::CompoundAvrg] = &shaders->inter_comp;
      shaders_inter[InterTypes::CompoundDiff] = &shaders->inter_comp_diff_y;
      shaders_inter[InterTypes::CompoundMasked] = &shaders->inter_comp_masked;
      shaders_inter[InterTypes::CompoundDiffUv] = &shaders->inter_comp_diff_uv;
      shaders_inter[InterTypes::ObmcAbove] = &shaders->inter_obmc_above;
      shaders_inter[InterTypes::ObmcLeft] = &shaders->inter_obmc_left;
      shaders_inter[InterTypes::Inter2x2] = &shaders->inter_2x2;
      shaders_inter[InterTypes::CompoundGlobalWarp] = &shaders->inter_warp_comp;
      shaders_inter[InterTypes::CompoundDiffUvGlobalWarp] = &shaders->inter_warp_comp;
    }

    ComputeShader *shader = &shaders->inter_base;
    command_list->SetComputeRootSignature(shader->signaturePtr.Get());
    command_list->SetComputeRootShaderResourceView(0, dec->prediction_blocks->dev->GetGPUVirtualAddress());
    command_list->SetComputeRootShaderResourceView(1, dec->idct_residuals->dev->GetGPUVirtualAddress());
    command_list->SetComputeRootShaderResourceView(2, dec->inter_mask_lut->dev->GetGPUVirtualAddress());
    command_list->SetComputeRootShaderResourceView(3, dec->prediction_blocks_warp->dev->GetGPUVirtualAddress());
    command_list->SetComputeRootShaderResourceView(4, dec->inter_warp_filter->dev->GetGPUVirtualAddress());
    command_list->SetComputeRootUnorderedAccessView(5, dec->frame_buffer_pool->dev->GetGPUVirtualAddress());
    command_list->SetComputeRootConstantBufferView(6, cbo.dev_address);
    PSSLInterSRT srt;
    int *offsets = NULL;
    shader = NULL;
    for (int t = InterTypes::Warp; t <= InterTypes::CompoundMasked; ++t) {
      const int wi_per_subb = t == InterTypes::Warp ? 1 : wi_per_block;
      offsets = (t == InterTypes::Warp) ? tiles->gen_block_map_wrp
                                        : (tiles->gen_block_map + (t - 1) * InterTypes::InterSizesAllCommon);
      const int is_4x2 = td->scale_enable == 0 && t > InterTypes::CasualInter;
      for (int i = 0; i < InterTypes::InterSizesAllCommon; ++i) {
        const int offset = offsets[i];
        const int count = offsets[i + 1] - offset;
        if (!count) continue;
        srt.pass_offset = offset;
        srt.width_log2 = InterBlockWidthLUT[i];
        srt.height_log2 = InterBlockHeightLUT[i] + is_4x2;
        srt.wi_count = (wi_per_subb << (srt.width_log2 + srt.height_log2)) * count;

        if (shader != shaders_inter[t]) {
          shader = shaders_inter[t];
          command_list->SetPipelineState(shader->pso.Get());
        }
        command_list->SetComputeRoot32BitConstants(7, 4, &srt, 0);
        command_list->Dispatch((srt.wi_count + 63) >> 6, 1, 1);
      }
    }

    if (enable_gm_warp) {
      offsets = tiles->gen_block_map + (InterTypes::CompoundGlobalWarp - 1) * InterTypes::InterSizesAllCommon;
      for (int i = 0; i < InterTypes::InterSizesAllCommon; ++i) {
        const int offset = offsets[i];
        const int count = offsets[i + 1] - offset;
        if (!count) continue;
        srt.pass_offset = offset;
        srt.width_log2 = InterBlockWidthLUT[i];
        srt.height_log2 = InterBlockHeightLUT[i];
        srt.wi_count = (4 << (srt.width_log2 + srt.height_log2)) * count;

        if (shader != &shaders->inter_warp_comp) {
          shader = &shaders->inter_warp_comp;
          command_list->SetPipelineState(shader->pso.Get());
        }
        command_list->SetComputeRoot32BitConstants(7, 4, &srt, 0);
        command_list->Dispatch((srt.wi_count + 63) >> 6, 1, 1);
      }
    }

    if (tiles->gen_block_map[InterTypes::Inter2x2ArrOffset + 1] > tiles->gen_block_map[InterTypes::Inter2x2ArrOffset]) {
      shader = shaders_inter[InterTypes::Inter2x2];
      const int offset = tiles->gen_block_map[InterTypes::Inter2x2ArrOffset];
      const int count = tiles->gen_block_map[InterTypes::Inter2x2ArrOffset + 1] - offset;
      srt.pass_offset = offset;
      srt.width_log2 = 0;
      srt.height_log2 = 0;
      srt.wi_count = count << td->scale_enable;
      command_list->SetPipelineState(shader->pso.Get());
      command_list->SetComputeRoot32BitConstants(7, 4, &srt, 0);
      command_list->Dispatch((srt.wi_count + 63) >> 6, 1, 1);
    }

    // sync 0
    command_list->ResourceBarrier(1, &CD3DX12_RESOURCE_BARRIER::UAV(dec->frame_buffer_pool->dev));

    offsets = tiles->gen_block_map + (InterTypes::CompoundDiffUv - 1) * InterTypes::InterSizesAllCommon;
    for (int i = 0; i < InterTypes::InterSizesAllCommon; ++i) {
      const int offset = offsets[i];
      const int count = offsets[i + 1] - offset;
      if (!count) continue;
      srt.pass_offset = offset;
      srt.width_log2 = InterBlockWidthLUT[i];
      srt.height_log2 = InterBlockHeightLUT[i] + (td->scale_enable == 0);
      srt.wi_count = (wi_per_block << (srt.width_log2 + srt.height_log2)) * count;
      if (shader != shaders_inter[InterTypes::CompoundDiffUv]) {
        shader = shaders_inter[InterTypes::CompoundDiffUv];
        command_list->SetPipelineState(shader->pso.Get());
      }
      command_list->SetComputeRoot32BitConstants(7, 4, &srt, 0);
      command_list->Dispatch((srt.wi_count + 63) >> 6, 1, 1);
    }

    if (enable_gm_warp) {
      offsets = tiles->gen_block_map + (InterTypes::CompoundDiffUvGlobalWarp - 1) * InterTypes::InterSizesAllCommon;
      for (int i = 0; i < InterTypes::InterSizesAllCommon; ++i) {
        const int offset = offsets[i];              // i == 0 ? offset0 : offsets[i - 1];
        const int count = offsets[i + 1] - offset;  // offsets[i] - offset;
        if (!count) continue;
        srt.pass_offset = offset;
        srt.width_log2 = InterBlockWidthLUT[i];
        srt.height_log2 = InterBlockHeightLUT[i];
        srt.wi_count = (4 << (srt.width_log2 + srt.height_log2)) * count;
        if (shader != &shaders->inter_warp_comp) {
          shader = &shaders->inter_warp_comp;
          command_list->SetPipelineState(shader->pso.Get());
        }
        command_list->SetComputeRoot32BitConstants(7, 4, &srt, 0);
        command_list->Dispatch((srt.wi_count + 63) >> 6, 1, 1);
      }
    }

    offsets = tiles->gen_block_map + (InterTypes::ObmcAbove - 1) * InterTypes::InterSizesAllCommon;
    for (int i = 0; i < InterTypes::InterSizesAllCommon; ++i) {
      const int offset = offsets[i];
      const int count = offsets[i + 1] - offset;
      if (!count) continue;
      srt.pass_offset = offset;  //((w << 2) | h)
      srt.width_log2 = i >> 2;
      srt.height_log2 = i & 3;
      srt.wi_count = (wi_per_block << (srt.width_log2 + srt.height_log2)) * count;
      if (shader != shaders_inter[InterTypes::ObmcAbove]) {
        shader = shaders_inter[InterTypes::ObmcAbove];
        command_list->SetPipelineState(shader->pso.Get());
      }
      command_list->SetComputeRoot32BitConstants(7, 4, &srt, 0);
      command_list->Dispatch((srt.wi_count + 63) >> 6, 1, 1);
    }

    // sync 1
    command_list->ResourceBarrier(1, &CD3DX12_RESOURCE_BARRIER::UAV(dec->frame_buffer_pool->dev));

    // pass 2: Obmc-left
    offsets = tiles->gen_block_map + (ObmcLeft - 1) * InterTypes::InterSizesAllCommon;
    for (int i = 0; i < InterTypes::InterSizesAllCommon; ++i) {
      const int offset = offsets[i];
      const int count = offsets[i + 1] - offset;
      if (!count) continue;
      srt.pass_offset = offset;
      srt.width_log2 = i & 3;
      srt.height_log2 = i >> 2;
      srt.wi_count = (wi_per_block << (srt.width_log2 + srt.height_log2)) * count;
      if (shader != shaders_inter[InterTypes::ObmcLeft]) {
        shader = shaders_inter[InterTypes::ObmcLeft];
        command_list->SetPipelineState(shader->pso.Get());
      }
      command_list->SetComputeRoot32BitConstants(7, 4, &srt, 0);
      command_list->Dispatch((srt.wi_count + 63) >> 6, 1, 1);
    }

    command_list->ResourceBarrier(1, &CD3DX12_RESOURCE_BARRIER::UAV(dec->frame_buffer_pool->dev));
    av1_perf_gpu_mark(dec, AV1_PERF_GPU_INTER);
    av1_stage_dump_fb(dec, "inter", td->frame_buffer);
  }

  const int recon_blocks =
      tiles->gen_block_map[ReconBlockOffset + ReconstructBlockSizes] - tiles->gen_block_map[ReconBlockOffset];
  if (td->intra_iters >= 0 || recon_blocks) {
    ConstantBufferObject cbo = cb->Alloc(sizeof(PSSLIntraData));
    PSSLIntraData *data = (PSSLIntraData *)cbo.host_ptr;
    // The /Od build unless -intraopt asks for the optimised build of the same source (same root signature).
    ComputeShader *const intra_main = g_intra_opt ? &shaders->intra_main_opt : &shaders->intra_main;
    ComputeShader *shader = intra_main;

    data->flags[0] = cm->seq_params.enable_intra_edge_filter;
    data->flags[1] = g_intra_edge_fix;
    data->flags[2] = 0;
    data->flags[3] = 0;
    memcpy(data->planes, td->frame_buffer->planes, sizeof(data->planes));
    for (int m = 0; m < 5; ++m)
      for (int k = 0; k < 8; ++k)
        for (int i = 0; i < 8; ++i) data->filter[m][k][i] = av1_filter_intra_taps[m][k][i];
    memcpy(data->mode_params_lut, intra_mode_shader_params, sizeof(intra_mode_shader_params));
    for (int i = 0; i < 128; ++i) data->sm_weight_arrays[i][0] = sm_weight_arrays[i];

    command_list->SetComputeRootSignature(shader->signaturePtr.Get());
    command_list->SetComputeRootShaderResourceView(0, dec->prediction_blocks->dev->GetGPUVirtualAddress());
    command_list->SetComputeRootShaderResourceView(1, dec->idct_residuals->dev->GetGPUVirtualAddress());
    command_list->SetComputeRootUnorderedAccessView(3, dec->frame_buffer_pool->dev->GetGPUVirtualAddress());
    command_list->SetComputeRootConstantBufferView(4, cbo.dev_address);
    if (recon_blocks) {
      ComputeShader *shader = &shaders->reconstruct_block;
      command_list->SetPipelineState(shader->pso.Get());
      // A slot has a palette buffer once a frame that allows screen content tools has used it. Without one
      // the frame has no palette block, and the shader loads from this buffer for palette blocks only; the
      // binding still needs a buffer, so it gets another upload buffer of the slot.
      const GpuBufferObject *palette = td->palette_buffer ? td->palette_buffer : td->idct_blocks_unordered;
      command_list->SetComputeRootShaderResourceView(2, palette->dev->GetGPUVirtualAddress());
      PSSLReconSRT recon_srt;
      recon_srt.fb_base_offset = static_cast<int>(td->frame_buffer->base_offset);
      for (int i = 0; i < ReconstructBlockSizes; ++i) {
        int offset = tiles->gen_block_map[ReconBlockOffset + i];
        int count = tiles->gen_block_map[ReconBlockOffset + i + 1] - offset;
        if (count) {
          recon_srt.pass_offset = offset;
          recon_srt.width_log2 = i % 6;
          recon_srt.height_log2 = i / 6;
          recon_srt.wi_count = count << (recon_srt.width_log2 + recon_srt.height_log2);

          command_list->SetComputeRoot32BitConstants(5, 10, &recon_srt, 0);
          command_list->Dispatch((recon_srt.wi_count + 63) >> 6, 1, 1);
        }
      }

      command_list->ResourceBarrier(1, &CD3DX12_RESOURCE_BARRIER::UAV(dec->frame_buffer_pool->dev));
      av1_perf_gpu_mark(dec, AV1_PERF_GPU_RECON);
      av1_stage_dump_fb(dec, "recon", td->frame_buffer);
    }

    if (td->intra_iters >= 0 && td->intra_v2) {
      // -intrav2: one thread group per region entry (a 64x64 region in one step), the wavefront inside a region
      // runs in the group. A UAV barrier is only needed between steps: groups of one step never read each
      // other's pixels.
      IntraV2Sched *sched = &td->intra_v2_sched;
      const int entries = (int)td->intra_v2_list_count;
      intra_v2_sort(sched->list, entries);
      ComputeShader *v2_shader =
          td->is_hbd ? &dec->shader_lib->shader_intra_v2_hbd : &dec->shader_lib->shader_intra_v2;
      command_list->SetPipelineState(v2_shader->pso.Get());
      command_list->SetComputeRootSignature(v2_shader->signaturePtr.Get());
      command_list->SetComputeRootShaderResourceView(0, dec->prediction_blocks->dev->GetGPUVirtualAddress());
      command_list->SetComputeRootShaderResourceView(1, dec->idct_residuals->dev->GetGPUVirtualAddress());
      command_list->SetComputeRootShaderResourceView(2, dec->inter_mask_lut->dev->GetGPUVirtualAddress());
      command_list->SetComputeRootShaderResourceView(3, td->intra_v2_buffer->dev->GetGPUVirtualAddress());
      command_list->SetComputeRootShaderResourceView(4, td->gen_block_map->dev->GetGPUVirtualAddress());
      command_list->SetComputeRootUnorderedAccessView(5, dec->frame_buffer_pool->dev->GetGPUVirtualAddress());
      command_list->SetComputeRootConstantBufferView(6, cbo.dev_address);
      const unsigned int list_offset = (unsigned int)((const char *)sched->list - (const char *)sched->cells);
      int steps = 0;
      IntraV2Dispatch d;
      for (int first = 0; intra_v2_next_dispatch(sched->list, entries, first, &d); first += d.count) {
        unsigned int v2_srt[8] = {0, d.step, d.passes, sched->stamp, (unsigned int)sched->map_stride,
                                  (unsigned int)sched->map_uv_offset, 0, 0};
        // A dispatch takes at most 65535 groups per dimension.
        for (int done = 0; done < d.count; done += 65535) {
          const int groups = AOMMIN(d.count - done, 65535);
          v2_srt[0] = list_offset + 8 * (unsigned int)(d.first + done);
          command_list->SetComputeRoot32BitConstants(7, 8, v2_srt, 0);
          command_list->Dispatch(groups, 1, 1);
          ++td->intra_stats[0];
        }
        td->intra_stats[2] += d.count;
        td->intra_stats[3] += d.count * (int)d.passes;
        if (d.last_of_step) {
          command_list->ResourceBarrier(1, &CD3DX12_RESOURCE_BARRIER::UAV(dec->frame_buffer_pool->dev));
          ++td->intra_stats[1];
          ++steps;
        }
      }
      td->intra_iters = steps - 1;  // for the timing record: the last step, like the last iteration
      av1_perf_gpu_mark(dec, AV1_PERF_GPU_INTRA);
      av1_stage_dump_fb(dec, "intra", td->frame_buffer);
    } else if (td->intra_iters >= 0) {
      PSSLIntraSRT srt;
      command_list->SetComputeRootShaderResourceView(2, dec->inter_mask_lut->dev->GetGPUVirtualAddress());
      const int *offsets = tiles->gen_block_map + InterTypes::InterCountsAll + ReconstructBlockSizes;
      ComputeShader *shader = NULL;
      for (int it = 0; it <= td->intra_iters; ++it) {
        const int base = it * IntraTypeCount + 1;
        int offset = offsets[-1 + base];
        srt.counts0[0] = offsets[base] - offset;
        srt.counts0[1] = offsets[base + 1] - offsets[base + 0];
        srt.counts0[2] = offsets[base + 2] - offsets[base + 1];
        srt.counts0[3] = offsets[base + 3] - offsets[base + 2];
        srt.counts0[4] = offsets[base + 4] - offsets[base + 3];
        srt.counts0[5] = offsets[base + 5] - offsets[base + 4];
        srt.counts0[6] = offsets[base + 6] - offsets[base + 5];
        srt.counts0[7] = offsets[base + 7] - offsets[base + 6];
        int counts8 = offsets[base + 8] - offsets[base + 7];
        srt.pass_offset = offset;
        srt.wi_count = (srt.counts0[0] << 10) + (srt.counts0[1] << 9) + (srt.counts0[2] << 8) + (srt.counts0[3] << 7) +
                       (srt.counts0[4] << 6) + (srt.counts0[5] << 5) + (srt.counts0[6] << 4) + (srt.counts0[7] << 3) +
                       (counts8 << 2);
        if (srt.wi_count) {
          if (shader != intra_main) {
            shader = intra_main;
            command_list->SetPipelineState(shader->pso.Get());
          }
          command_list->SetComputeRoot32BitConstants(5, 10, &srt, 0);
          command_list->Dispatch((srt.wi_count + 255) >> 8, 1, 1);
          ++td->intra_stats[0];
          td->intra_stats[2] += (srt.wi_count + 255) >> 8;
        }

        offset = offsets[base + IntraSizes - 1];
        int count = offsets[base + IntraSizes] - offset;
        if (count) {
          int filt_srt[2];
          filt_srt[0] = count << 3;
          filt_srt[1] = offset;
          if (shader != &shaders->intra_filter) {
            shader = &shaders->intra_filter;
            command_list->SetPipelineState(shader->pso.Get());
          }
          command_list->SetComputeRoot32BitConstants(5, 2, filt_srt, 0);
          command_list->Dispatch(count, 1, 1);
          ++td->intra_stats[0];
          td->intra_stats[2] += count;
        }

        if (count || srt.wi_count) {
          command_list->ResourceBarrier(1, &CD3DX12_RESOURCE_BARRIER::UAV(dec->frame_buffer_pool->dev));
          ++td->intra_stats[1];
        } else {
          assert(0);
        }
      }
      td->intra_stats[3] = td->intra_stats[2];  // one pass per thread group
      av1_perf_gpu_mark(dec, AV1_PERF_GPU_INTRA);
      av1_stage_dump_fb(dec, "intra", td->frame_buffer);
    }
  }
  PutPerfMarker(td, &td->perf_markers[3]);
}

void av1_inter_ext_borders(Av1Core *dec, AV1_COMMON *cm) {
  av1_frame_thread_data *td = dec->curr_frame_data;
  ComputeCommandBuffer *cb = &td->command_buffer;

  typedef struct {
    int planes[3][4];
    int dims[2][4];
  } CBData;

  ConstantBufferObject cbo = cb->Alloc(sizeof(CBData));
  CBData *cb_data = static_cast<CBData *>(cbo.host_ptr);
  memcpy(cb_data->planes, td->dst_frame_buffer->planes, sizeof(cb_data->planes));
  YV12_BUFFER_CONFIG *buf = &cm->cur_frame->buf;

  cb_data->dims[0][0] = buf->y_crop_width;
  cb_data->dims[0][1] = buf->y_crop_height;
  cb_data->dims[1][0] = buf->uv_crop_width;
  cb_data->dims[1][1] = buf->uv_crop_height;

  ComputeShader *shader = &td->shaders->inter_ext_borders;
  const int wi_count = buf->y_crop_height + buf->uv_crop_height * 2;
  Microsoft::WRL::ComPtr<ID3D12GraphicsCommandList> command_list = dec->compute.command_list;
  command_list->SetComputeRootSignature(shader->signaturePtr.Get());
  command_list->SetPipelineState(shader->pso.Get());
  command_list->SetComputeRootUnorderedAccessView(0, dec->frame_buffer_pool->dev->GetGPUVirtualAddress());
  command_list->SetComputeRootConstantBufferView(1, cbo.dev_address);
  command_list->Dispatch((wi_count + 63) / 64, 1, 1);
  command_list->ResourceBarrier(1, &CD3DX12_RESOURCE_BARRIER::UAV(dec->frame_buffer_pool->dev));
  av1_perf_gpu_mark(dec, AV1_PERF_GPU_EXT_BORDERS);
  av1_stage_dump_fb(dec, "borders", td->dst_frame_buffer);
}
