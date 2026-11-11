/*
 * Copyright (c) 2016, Alliance for Open Media. All rights reserved
 *
 * This source code is subject to the terms of the BSD 2 Clause License and
 * the Alliance for Open Media Patent License 1.0. If the BSD 2 Clause License
 * was not distributed with this source code in the LICENSE file, you can
 * obtain it at www.aomedia.org/license/software. If the Alliance for Open
 * Media Patent License 1.0 was not distributed with this source code in the
 * PATENTS file, you can obtain it at www.aomedia.org/license/patent.
 */

#include <assert.h>
#include <stddef.h>

#include "config/aom_config.h"
#include "config/aom_dsp_rtcd.h"
#include "config/aom_scale_rtcd.h"
#include "config/av1_rtcd.h"

#include "aom/aom_codec.h"
#include "aom_dsp/aom_dsp_common.h"
#include "aom_dsp/binary_codes_reader.h"
#include "aom_dsp/bitreader.h"
#include "aom_dsp/bitreader_buffer.h"
#include "aom_mem/aom_mem.h"
#include "aom_ports/aom_timer.h"
#include "aom_ports/mem.h"
#include "aom_ports/mem_ops.h"
#include "aom_scale/aom_scale.h"
#include "aom_util/aom_thread.h"

#if CONFIG_BITSTREAM_DEBUG || CONFIG_MISMATCH_DEBUG
#include "aom_util/debug_util.h"
#endif  // CONFIG_BITSTREAM_DEBUG || CONFIG_MISMATCH_DEBUG

#include "av1/common/alloccommon.h"
#include "av1/common/cdef.h"
#include "av1/common/cfl.h"
#if CONFIG_INSPECTION
#include "av1/decoder/inspection.h"
#endif
#include "av1/common/common.h"
#include "av1/common/entropy.h"
#include "av1/common/entropymode.h"
#include "av1/common/entropymv.h"
#include "av1/common/frame_buffers.h"
#include "av1/common/idct.h"
#include "av1/common/mvref_common.h"
#include "av1/common/pred_common.h"
#include "av1/common/quant_common.h"
#include "av1/common/reconinter.h"
#include "av1/common/reconintra.h"
#include "av1/common/resize.h"
#include "av1/common/seg_common.h"
#include "av1/common/thread_common.h"
#include "av1/common/tile_common.h"
#include "av1/common/warped_motion.h"
#include "av1/common/obmc.h"
#include "av1/decoder/decodeframe.h"
#include "av1/decoder/decodemv.h"
#include "av1/decoder/decoder.h"
#include "av1/decoder/decodetxb.h"
#include "av1/decoder/detokenize.h"


#include "dx\av1_core.h"


#define ACCT_STR __func__

#define AOM_MIN_THREADS_PER_TILE 1
#define AOM_MAX_THREADS_PER_TILE 2

// This is needed by ext_tile related unit tests.
#define EXT_TILE_DEBUG 1
#define MC_TEMP_BUF_PELS                       \
  (((MAX_SB_SIZE)*2 + (AOM_INTERP_EXTEND)*2) * \
   ((MAX_SB_SIZE)*2 + (AOM_INTERP_EXTEND)*2))

// Checks that the remaining bits start with a 1 and ends with 0s.
// It consumes an additional byte, if already byte aligned before the check.
int av1_check_trailing_bits(AV1Decoder *pbi, struct aom_read_bit_buffer *rb) {
  AV1_COMMON *const cm = &pbi->common;
  // bit_offset is set to 0 (mod 8) when the reader is already byte aligned
  int bits_before_alignment = 8 - rb->bit_offset % 8;
  int trailing = aom_rb_read_literal(rb, bits_before_alignment);
  if (trailing != (1 << (bits_before_alignment - 1))) {
    cm->error.error_code = AOM_CODEC_CORRUPT_FRAME;
    return -1;
  }
  return 0;
}

// Use only_chroma = 1 to only set the chroma planes
static void set_planes_to_neutral_grey(const SequenceHeader *const seq_params,
                                       const YV12_BUFFER_CONFIG *const buf,
                                       int only_chroma) {
  HwFrameBuffer * hbuf = buf->hw_buffer;
//  if (hbuf)
//    return;
//  if (seq_params->use_highbitdepth) {
//    const int val = 1 << (seq_params->bit_depth - 1);
//    for (int plane = only_chroma; plane < MAX_MB_PLANE; plane++) {
//      uint16_t *const base = (uint16_t*)(hbuf->pool_ptr + hbuf->planes[plane].offset);
//      const int sz = (plane ? hbuf->uv_crop_height : hbuf->y_crop_height) * hbuf->planes[plane].stride;
//      if (sz > 0)
//        aom_memset16(base, val, sz);
//    }
//  } else {
//    for (int plane = only_chroma; plane < MAX_MB_PLANE; plane++) {
//      int8_t *const base = hbuf->pool_ptr + hbuf->planes[plane].offset;
//      const int sz = (plane ? hbuf->uv_crop_height : hbuf->y_crop_height) * hbuf->planes[plane].stride;
//      if (sz > 0)
//          memset(base, 1 << 7, sz);
//    }
//  }
}

static void loop_restoration_read_sb_coeffs(const AV1_COMMON *const cm,
                                            MACROBLOCKD *xd,
                                            aom_reader *const r, int plane,
                                            int runit_idx);

static void setup_compound_reference_mode(AV1_COMMON *cm) {
  cm->comp_fwd_ref[0] = LAST_FRAME;
  cm->comp_fwd_ref[1] = LAST2_FRAME;
  cm->comp_fwd_ref[2] = LAST3_FRAME;
  cm->comp_fwd_ref[3] = GOLDEN_FRAME;

  cm->comp_bwd_ref[0] = BWDREF_FRAME;
  cm->comp_bwd_ref[1] = ALTREF2_FRAME;
  cm->comp_bwd_ref[2] = ALTREF_FRAME;
}

static int read_is_valid(const uint8_t *start, size_t len, const uint8_t *end) {
  return len != 0 && len <= (size_t)(end - start);
}

static TX_MODE read_tx_mode(AV1_COMMON *cm, struct aom_read_bit_buffer *rb) {
  if (cm->coded_lossless) return ONLY_4X4;
  return aom_rb_read_bit(rb) ? TX_MODE_SELECT : TX_MODE_LARGEST;
}

static REFERENCE_MODE read_frame_reference_mode(
    const AV1_COMMON *cm, struct aom_read_bit_buffer *rb) {
  if (frame_is_intra_only(cm)) {
    return SINGLE_REFERENCE;
  } else {
    return aom_rb_read_bit(rb) ? REFERENCE_MODE_SELECT : SINGLE_REFERENCE;
  }
}

static void read_coeffs_tx_intra_block(const AV1_COMMON *const cm,
                                       MACROBLOCKD *const xd,
                                       aom_reader *const r, const int plane,
                                       const int row, const int col,
                                       const TX_SIZE tx_size) {
  MB_MODE_INFO *mbmi = xd->mi[0];
  if (!mbmi->skip) {
#if TXCOEFF_TIMER
    struct aom_usec_timer timer;
    aom_usec_timer_start(&timer);
#endif
    av1_read_coeffs_txb_facade(cm, xd, r, plane, row, col, tx_size);
#if TXCOEFF_TIMER
    aom_usec_timer_mark(&timer);
    const int64_t elapsed_time = aom_usec_timer_elapsed(&timer);
    cm->txcoeff_timer += elapsed_time;
    ++cm->txb_count;
#endif
  }
}

static void decode_block_void(const AV1_COMMON *const cm, MACROBLOCKD *const xd,
                              aom_reader *const r, const int plane,
                              const int row, const int col,
                              const TX_SIZE tx_size) {
  (void)cm;
  (void)xd;
  (void)r;
  (void)plane;
  (void)row;
  (void)col;
  (void)tx_size;
}

static void predict_inter_block_void(AV1_COMMON *const cm,
                                     MACROBLOCKD *const xd, int mi_row,
                                     int mi_col, BLOCK_SIZE bsize) {
  (void)cm;
  (void)xd;
  (void)mi_row;
  (void)mi_col;
  (void)bsize;
}

static void cfl_store_inter_block_void(AV1_COMMON *const cm,
                                       MACROBLOCKD *const xd) {
  (void)cm;
  (void)xd;
}

static void set_cb_buffer_offsets(MACROBLOCKD *const xd, TX_SIZE tx_size,
                                  int plane) {
  xd->cb_offset[plane] += tx_size_wide[tx_size] * tx_size_high[tx_size];
  xd->txb_offset[plane] =
      xd->cb_offset[plane] / (TX_SIZE_W_MIN * TX_SIZE_H_MIN);
}

static void decode_reconstruct_tx(AV1_COMMON *cm, ThreadData *const td,
                                  aom_reader *r, MB_MODE_INFO *const mbmi,
                                  int plane, BLOCK_SIZE plane_bsize,
                                  int blk_row, int blk_col, int block,
                                  TX_SIZE tx_size, int *eob_total) {
  MACROBLOCKD *const xd = &td->xd;
  const struct macroblockd_plane *const pd = &xd->plane[plane];
  const TX_SIZE plane_tx_size =
      plane ? av1_get_max_uv_txsize(mbmi->sb_type, pd->subsampling_x,
                                    pd->subsampling_y)
            : mbmi->inter_tx_size[av1_get_txb_size_index(plane_bsize, blk_row,
                                                         blk_col)];
  // Scale to match transform block unit.
  const int max_blocks_high = max_block_high(xd, plane_bsize, plane);
  const int max_blocks_wide = max_block_wide(xd, plane_bsize, plane);

  if (blk_row >= max_blocks_high || blk_col >= max_blocks_wide) return;

  if (tx_size == plane_tx_size || plane) {
    td->read_coeffs_tx_inter_block_visit(cm, xd, r, plane, blk_row, blk_col,
                                         tx_size);

    td->inverse_tx_inter_block_visit(cm, xd, r, plane, blk_row, blk_col,
                                     tx_size);
    eob_info *eob_data = pd->eob_data + xd->txb_offset[plane];
    *eob_total += eob_data->eob;
    set_cb_buffer_offsets(xd, tx_size, plane);
  } else {
    const TX_SIZE sub_txs = sub_tx_size_map[tx_size];
    assert(IMPLIES(tx_size <= TX_4X4, sub_txs == tx_size));
    assert(IMPLIES(tx_size > TX_4X4, sub_txs < tx_size));
    const int bsw = tx_size_wide_unit[sub_txs];
    const int bsh = tx_size_high_unit[sub_txs];
    const int sub_step = bsw * bsh;

    assert(bsw > 0 && bsh > 0);

    for (int row = 0; row < tx_size_high_unit[tx_size]; row += bsh) {
      for (int col = 0; col < tx_size_wide_unit[tx_size]; col += bsw) {
        const int offsetr = blk_row + row;
        const int offsetc = blk_col + col;

        if (offsetr >= max_blocks_high || offsetc >= max_blocks_wide) continue;

        decode_reconstruct_tx(cm, td, r, mbmi, plane, plane_bsize, offsetr,
                              offsetc, block, sub_txs, eob_total);
        block += sub_step;
      }
    }
  }
}


static void set_offsets(AV1_COMMON *const cm, MACROBLOCKD *const xd,
                        BLOCK_SIZE bsize, int mi_row, int mi_col, int bw,
                        int bh, int x_mis, int y_mis, ThreadData *const td) {
  const int num_planes = av1_num_planes(cm);

  const int offset = mi_row * cm->mi_stride + mi_col;
  const TileInfo *const tile = &xd->tile;

  xd->mi = cm->mi_grid_visible + offset;
  if (td->mi_pool2) {
    xd->mi[0] = &td->mi_pool2[td->mi_count2++];
  } else {
    xd->mi[0] = &td->mi_pool[td->mi_count++];
  }
  // TODO(slavarnway): Generate sb_type based on bwl and bhl, instead of
  // passing bsize from decode_partition().
  xd->mi[0]->sb_type = bsize;
  xd->mi[0]->mi_row = mi_row;
  xd->mi[0]->mi_col = mi_col;
  xd->cfl.mi_row = mi_row;
  xd->cfl.mi_col = mi_col;

  assert(x_mis && y_mis);
  for (int x = 1; x < x_mis; ++x) xd->mi[x] = xd->mi[0];
  int idx = cm->mi_stride;
  for (int y = 1; y < y_mis; ++y) {
    memcpy(&xd->mi[idx], &xd->mi[0], x_mis * sizeof(xd->mi[0]));
    idx += cm->mi_stride;
  }

  set_plane_n4(xd, bw, bh, num_planes);
  set_skip_context(xd, mi_row, mi_col, num_planes);

  // Distance of Mb to the various image edges. These are specified to 8th pel
  // as they are always compared to values that are in 1/8th pel units
  set_mi_row_col(xd, tile, mi_row, bh, mi_col, bw, cm->mi_rows, cm->mi_cols);

  av1_setup_dst_planes(xd->plane, bsize, &cm->cur_frame->buf, mi_row, mi_col, 0,
                       num_planes);
}

static void decode_mbmi_block(AV1Decoder *const pbi, MACROBLOCKD *const xd,
                              int mi_row, int mi_col, aom_reader *r,
                              PARTITION_TYPE partition, BLOCK_SIZE bsize,
                              ThreadData *const td) {
  AV1_COMMON *const cm = &pbi->common;
  const SequenceHeader *const seq_params = &cm->seq_params;
  const int bw = mi_size_wide[bsize];
  const int bh = mi_size_high[bsize];
  const int x_mis = AOMMIN(bw, cm->mi_cols - mi_col);
  const int y_mis = AOMMIN(bh, cm->mi_rows - mi_row);

#if CONFIG_ACCOUNTING
  aom_accounting_set_context(&pbi->accounting, mi_col, mi_row);
#endif
  if (td->mi_count >= td->mi_count_max && td->mi_pool2 == NULL) {
      av1_setup_sec_data(pbi, cm, td);
  }
  set_offsets(cm, xd, bsize, mi_row, mi_col, bw, bh, x_mis, y_mis, td);
  xd->mi[0]->partition = partition;
  av1_read_mode_info(pbi, xd, mi_row, mi_col, r, x_mis, y_mis);
  if (bsize >= BLOCK_8X8 &&
      (seq_params->subsampling_x || seq_params->subsampling_y)) {
    const BLOCK_SIZE uv_subsize =
        ss_size_lookup[bsize][seq_params->subsampling_x]
                      [seq_params->subsampling_y];
    if (uv_subsize == BLOCK_INVALID)
      aom_internal_error(xd->error_info, AOM_CODEC_CORRUPT_FRAME,
                         "Invalid block size.");
  }
}

static void set_color_index_map_offset(AV1Decoder * pbi, MACROBLOCKD *const xd, int plane,
                                       aom_reader *r) {
  (void)r;
  Av1ColorMapParam params;
  const MB_MODE_INFO *const mbmi = xd->mi[0];
  av1_get_block_dimensions(mbmi->sb_type, plane, xd, &params.plane_width,
                           &params.plane_height, NULL, NULL);
  xd->color_index_map_offset[plane] += params.plane_width * params.plane_height;
}

static void decode_token_recon_block(AV1Decoder *const pbi,
                                     ThreadData *const td, int mi_row,
                                     int mi_col, aom_reader *r,
                                     BLOCK_SIZE bsize) {
  AV1_COMMON *const cm = &pbi->common;
  MACROBLOCKD *const xd = &td->xd;
  const int num_planes = av1_num_planes(cm);

  MB_MODE_INFO *mbmi = xd->mi[0];
  CFL_CTX *const cfl = &xd->cfl;
  cfl->is_chroma_reference = is_chroma_reference(
      mi_row, mi_col, bsize, cfl->subsampling_x, cfl->subsampling_y);

  if (!is_inter_block(mbmi)) {
    int row, col;
    assert(bsize == get_plane_block_size(bsize, xd->plane[0].subsampling_x,
                                         xd->plane[0].subsampling_y));
    const int max_blocks_wide = max_block_wide(xd, bsize, 0);
    const int max_blocks_high = max_block_high(xd, bsize, 0);
    const BLOCK_SIZE max_unit_bsize = BLOCK_64X64;
    int mu_blocks_wide =
        block_size_wide[max_unit_bsize] >> tx_size_wide_log2[0];
    int mu_blocks_high =
        block_size_high[max_unit_bsize] >> tx_size_high_log2[0];
    mu_blocks_wide = AOMMIN(max_blocks_wide, mu_blocks_wide);
    mu_blocks_high = AOMMIN(max_blocks_high, mu_blocks_high);

    for (row = 0; row < max_blocks_high; row += mu_blocks_high) {
      for (col = 0; col < max_blocks_wide; col += mu_blocks_wide) {
        for (int plane = 0; plane < num_planes; ++plane) {
          const struct macroblockd_plane *const pd = &xd->plane[plane];
          if (!is_chroma_reference(mi_row, mi_col, bsize, pd->subsampling_x,
                                   pd->subsampling_y))
            continue;

          const TX_SIZE tx_size = av1_get_tx_size(plane, xd);
          const int stepr = tx_size_high_unit[tx_size];
          const int stepc = tx_size_wide_unit[tx_size];

          const int unit_height = ROUND_POWER_OF_TWO(
              AOMMIN(mu_blocks_high + row, max_blocks_high), pd->subsampling_y);
          const int unit_width = ROUND_POWER_OF_TWO(
              AOMMIN(mu_blocks_wide + col, max_blocks_wide), pd->subsampling_x);

          for (int blk_row = row >> pd->subsampling_y; blk_row < unit_height;
               blk_row += stepr) {
            for (int blk_col = col >> pd->subsampling_x; blk_col < unit_width;
                 blk_col += stepc) {
              td->read_coeffs_tx_intra_block_visit(cm, xd, r, plane, blk_row,
                                                   blk_col, tx_size);
              td->predict_and_recon_intra_block_visit(cm, xd, r, plane, blk_row,
                                                      blk_col, tx_size);
              set_cb_buffer_offsets(xd, tx_size, plane);
            }
          }
        }
      }
    }
  } else {
    td->predict_inter_block_visit(cm, xd, mi_row, mi_col, bsize);
    // Reconstruction
    if (!mbmi->skip) {
      int eobtotal = 0;

      const int max_blocks_wide = max_block_wide(xd, bsize, 0);
      const int max_blocks_high = max_block_high(xd, bsize, 0);
      int row, col;

      const BLOCK_SIZE max_unit_bsize = BLOCK_64X64;
      assert(max_unit_bsize ==
             get_plane_block_size(BLOCK_64X64, xd->plane[0].subsampling_x,
                                  xd->plane[0].subsampling_y));
      int mu_blocks_wide =
          block_size_wide[max_unit_bsize] >> tx_size_wide_log2[0];
      int mu_blocks_high =
          block_size_high[max_unit_bsize] >> tx_size_high_log2[0];

      mu_blocks_wide = AOMMIN(max_blocks_wide, mu_blocks_wide);
      mu_blocks_high = AOMMIN(max_blocks_high, mu_blocks_high);

      for (row = 0; row < max_blocks_high; row += mu_blocks_high) {
        for (col = 0; col < max_blocks_wide; col += mu_blocks_wide) {
          for (int plane = 0; plane < num_planes; ++plane) {
            const struct macroblockd_plane *const pd = &xd->plane[plane];
            if (!is_chroma_reference(mi_row, mi_col, bsize, pd->subsampling_x,
                                     pd->subsampling_y))
              continue;
            const BLOCK_SIZE bsizec =
                scale_chroma_bsize(bsize, pd->subsampling_x, pd->subsampling_y);
            const BLOCK_SIZE plane_bsize = get_plane_block_size(
                bsizec, pd->subsampling_x, pd->subsampling_y);

            const TX_SIZE max_tx_size =
                get_vartx_max_txsize(xd, plane_bsize, plane);
            const int bh_var_tx = tx_size_high_unit[max_tx_size];
            const int bw_var_tx = tx_size_wide_unit[max_tx_size];
            int block = 0;
            int step =
                tx_size_wide_unit[max_tx_size] * tx_size_high_unit[max_tx_size];
            int blk_row, blk_col;
            const int unit_height = ROUND_POWER_OF_TWO(
                AOMMIN(mu_blocks_high + row, max_blocks_high),
                pd->subsampling_y);
            const int unit_width = ROUND_POWER_OF_TWO(
                AOMMIN(mu_blocks_wide + col, max_blocks_wide),
                pd->subsampling_x);

            for (blk_row = row >> pd->subsampling_y; blk_row < unit_height;
                 blk_row += bh_var_tx) {
              for (blk_col = col >> pd->subsampling_x; blk_col < unit_width;
                   blk_col += bw_var_tx) {
                decode_reconstruct_tx(cm, td, r, mbmi, plane, plane_bsize,
                                      blk_row, blk_col, block, max_tx_size,
                                      &eobtotal);
                block += step;
              }
            }
          }
        }
      }
    }
    td->cfl_store_inter_block_visit(cm, xd);
  }

  av1_visit_palette(pbi, xd, mi_row, mi_col, r, bsize,
                    set_color_index_map_offset);
}

#if LOOP_FILTER_BITMASK
static void store_bitmask_vartx(AV1_COMMON *cm, int mi_row, int mi_col,
                                BLOCK_SIZE bsize, TX_SIZE tx_size,
                                MB_MODE_INFO *mbmi);
#endif

static void set_inter_tx_size(MB_MODE_INFO *mbmi, int stride_log2,
                              int tx_w_log2, int tx_h_log2, int min_txs,
                              int split_size, int txs, int blk_row,
                              int blk_col) {
  for (int idy = 0; idy < tx_size_high_unit[split_size];
       idy += tx_size_high_unit[min_txs]) {
    for (int idx = 0; idx < tx_size_wide_unit[split_size];
         idx += tx_size_wide_unit[min_txs]) {
      const int index = (((blk_row + idy) >> tx_h_log2) << stride_log2) +
                        ((blk_col + idx) >> tx_w_log2);
      mbmi->inter_tx_size[index] = txs;
    }
  }
}

static void read_tx_size_vartx(MACROBLOCKD *xd, MB_MODE_INFO *mbmi,
                               TX_SIZE tx_size, int depth,
#if LOOP_FILTER_BITMASK
                               AV1_COMMON *cm, int mi_row, int mi_col,
                               int store_bitmask,
#endif
                               int blk_row, int blk_col, aom_reader *r) {
  FRAME_CONTEXT *ec_ctx = xd->tile_ctx;
  int is_split = 0;
  const BLOCK_SIZE bsize = mbmi->sb_type;
  const int max_blocks_high = max_block_high(xd, bsize, 0);
  const int max_blocks_wide = max_block_wide(xd, bsize, 0);
  if (blk_row >= max_blocks_high || blk_col >= max_blocks_wide) return;
  assert(tx_size > TX_4X4);
  TX_SIZE txs = max_txsize_rect_lookup[bsize];
  for (int level = 0; level < MAX_VARTX_DEPTH - 1; ++level)
    txs = sub_tx_size_map[txs];
  const int tx_w_log2 = tx_size_wide_log2[txs] - MI_SIZE_LOG2;
  const int tx_h_log2 = tx_size_high_log2[txs] - MI_SIZE_LOG2;
  const int bw_log2 = mi_size_wide_log2[bsize];
  const int stride_log2 = bw_log2 - tx_w_log2;

  if (depth == MAX_VARTX_DEPTH) {
    set_inter_tx_size(mbmi, stride_log2, tx_w_log2, tx_h_log2, txs, tx_size,
                      tx_size, blk_row, blk_col);
    mbmi->tx_size = tx_size;
    txfm_partition_update(xd->above_txfm_context + blk_col,
                          xd->left_txfm_context + blk_row, tx_size, tx_size);
    return;
  }

  const int ctx = txfm_partition_context(xd->above_txfm_context + blk_col,
                                         xd->left_txfm_context + blk_row,
                                         mbmi->sb_type, tx_size);
  is_split = aom_read_symbol(r, ec_ctx->txfm_partition_cdf[ctx], 2, ACCT_STR);

  if (is_split) {
    const TX_SIZE sub_txs = sub_tx_size_map[tx_size];
    const int bsw = tx_size_wide_unit[sub_txs];
    const int bsh = tx_size_high_unit[sub_txs];

    if (sub_txs == TX_4X4) {
      set_inter_tx_size(mbmi, stride_log2, tx_w_log2, tx_h_log2, txs, tx_size,
                        sub_txs, blk_row, blk_col);
      mbmi->tx_size = sub_txs;
      txfm_partition_update(xd->above_txfm_context + blk_col,
                            xd->left_txfm_context + blk_row, sub_txs, tx_size);
#if LOOP_FILTER_BITMASK
      if (store_bitmask) {
        store_bitmask_vartx(cm, mi_row + blk_row, mi_col + blk_col,
                            txsize_to_bsize[tx_size], TX_4X4, mbmi);
      }
#endif
      return;
    }
#if LOOP_FILTER_BITMASK
    if (depth + 1 == MAX_VARTX_DEPTH && store_bitmask) {
      store_bitmask_vartx(cm, mi_row + blk_row, mi_col + blk_col,
                          txsize_to_bsize[tx_size], sub_txs, mbmi);
      store_bitmask = 0;
    }
#endif

    assert(bsw > 0 && bsh > 0);
    for (int row = 0; row < tx_size_high_unit[tx_size]; row += bsh) {
      for (int col = 0; col < tx_size_wide_unit[tx_size]; col += bsw) {
        int offsetr = blk_row + row;
        int offsetc = blk_col + col;
        read_tx_size_vartx(xd, mbmi, sub_txs, depth + 1,
#if LOOP_FILTER_BITMASK
                           cm, mi_row, mi_col, store_bitmask,
#endif
                           offsetr, offsetc, r);
      }
    }
  } else {
    set_inter_tx_size(mbmi, stride_log2, tx_w_log2, tx_h_log2, txs, tx_size,
                      tx_size, blk_row, blk_col);
    mbmi->tx_size = tx_size;
    txfm_partition_update(xd->above_txfm_context + blk_col,
                          xd->left_txfm_context + blk_row, tx_size, tx_size);
#if LOOP_FILTER_BITMASK
    if (store_bitmask) {
      store_bitmask_vartx(cm, mi_row + blk_row, mi_col + blk_col,
                          txsize_to_bsize[tx_size], tx_size, mbmi);
    }
#endif
  }
}

static TX_SIZE read_selected_tx_size(MACROBLOCKD *xd, aom_reader *r) {
  // TODO(debargha): Clean up the logic here. This function should only
  // be called for intra.
  const BLOCK_SIZE bsize = xd->mi[0]->sb_type;
  const int32_t tx_size_cat = bsize_to_tx_size_cat(bsize);
  const int max_depths = bsize_to_max_depth(bsize);
  const int ctx = get_tx_size_context(xd);
  FRAME_CONTEXT *ec_ctx = xd->tile_ctx;
  const int depth = aom_read_symbol(r, ec_ctx->tx_size_cdf[tx_size_cat][ctx],
                                    max_depths + 1, ACCT_STR);
  assert(depth >= 0 && depth <= max_depths);
  const TX_SIZE tx_size = depth_to_tx_size(depth, bsize);
  return tx_size;
}

static TX_SIZE read_tx_size(AV1_COMMON *cm, MACROBLOCKD *xd, int is_inter,
                            int allow_select_inter, aom_reader *r) {
  const TX_MODE tx_mode = cm->tx_mode;
  const BLOCK_SIZE bsize = xd->mi[0]->sb_type;
  if (xd->lossless[xd->mi[0]->segment_id]) return TX_4X4;

  if (block_signals_txsize(bsize)) {
    if ((!is_inter || allow_select_inter) && tx_mode == TX_MODE_SELECT) {
      const TX_SIZE coded_tx_size = read_selected_tx_size(xd, r);
      return coded_tx_size;
    } else {
      return tx_size_from_tx_mode(bsize, tx_mode);
    }
  } else {
    assert(IMPLIES(tx_mode == ONLY_4X4, bsize == BLOCK_4X4));
    return max_txsize_rect_lookup[bsize];
  }
}

#if LOOP_FILTER_BITMASK
static void store_bitmask_vartx(AV1_COMMON *cm, int mi_row, int mi_col,
                                BLOCK_SIZE bsize, TX_SIZE tx_size,
                                MB_MODE_INFO *mbmi) {
  LoopFilterMask *lfm = get_loop_filter_mask(cm, mi_row, mi_col);
  const TX_SIZE tx_size_y_vert = txsize_vert_map[tx_size];
  const TX_SIZE tx_size_y_horz = txsize_horz_map[tx_size];
  const TX_SIZE tx_size_uv_vert = txsize_vert_map[av1_get_max_uv_txsize(
      mbmi->sb_type, cm->seq_params.subsampling_x,
      cm->seq_params.subsampling_y)];
  const TX_SIZE tx_size_uv_horz = txsize_horz_map[av1_get_max_uv_txsize(
      mbmi->sb_type, cm->seq_params.subsampling_x,
      cm->seq_params.subsampling_y)];
  const int is_square_transform_size = tx_size <= TX_64X64;
  int mask_id = 0;
  int offset = 0;
  const int half_ratio_tx_size_max32 =
      (tx_size > TX_64X64) & (tx_size <= TX_32X16);
  if (is_square_transform_size) {
    switch (tx_size) {
      case TX_4X4: mask_id = mask_id_table_tx_4x4[bsize]; break;
      case TX_8X8:
        mask_id = mask_id_table_tx_8x8[bsize];
        offset = 19;
        break;
      case TX_16X16:
        mask_id = mask_id_table_tx_16x16[bsize];
        offset = 33;
        break;
      case TX_32X32:
        mask_id = mask_id_table_tx_32x32[bsize];
        offset = 42;
        break;
      case TX_64X64: mask_id = 46; break;
      default: assert(!is_square_transform_size); return;
    }
    mask_id += offset;
  } else if (half_ratio_tx_size_max32) {
    int tx_size_equal_block_size = bsize == txsize_to_bsize[tx_size];
    mask_id = 47 + 2 * (tx_size - TX_4X8) + (tx_size_equal_block_size ? 0 : 1);
  } else if (tx_size == TX_32X64) {
    mask_id = 59;
  } else if (tx_size == TX_64X32) {
    mask_id = 60;
  } else {  // quarter ratio tx size
    mask_id = 61 + (tx_size - TX_4X16);
  }
  int index = 0;
  const int row = mi_row % MI_SIZE_64X64;
  const int col = mi_col % MI_SIZE_64X64;
  const int shift = get_index_shift(col, row, &index);
  const int vert_shift = tx_size_y_vert <= TX_8X8 ? shift : col;
  for (int i = 0; i + index < 4; ++i) {
    // y vertical.
    lfm->tx_size_ver[0][tx_size_y_horz].bits[i + index] |=
        (left_mask_univariant_reordered[mask_id].bits[i] << vert_shift);
    // y horizontal.
    lfm->tx_size_hor[0][tx_size_y_vert].bits[i + index] |=
        (above_mask_univariant_reordered[mask_id].bits[i] << shift);
    // u/v vertical.
    lfm->tx_size_ver[1][tx_size_uv_horz].bits[i + index] |=
        (left_mask_univariant_reordered[mask_id].bits[i] << vert_shift);
    // u/v horizontal.
    lfm->tx_size_hor[1][tx_size_uv_vert].bits[i + index] |=
        (above_mask_univariant_reordered[mask_id].bits[i] << shift);
  }
}

static void store_bitmask_univariant_tx(AV1_COMMON *cm, int mi_row, int mi_col,
                                        BLOCK_SIZE bsize, MB_MODE_INFO *mbmi) {
  // Use a lookup table that provides one bitmask for a given block size and
  // a univariant transform size.
  int index;
  int shift;
  int row;
  int col;
  LoopFilterMask *lfm = get_loop_filter_mask(cm, mi_row, mi_col);
  const TX_SIZE tx_size_y_vert = txsize_vert_map[mbmi->tx_size];
  const TX_SIZE tx_size_y_horz = txsize_horz_map[mbmi->tx_size];
  const TX_SIZE tx_size_uv_vert = txsize_vert_map[av1_get_max_uv_txsize(
      mbmi->sb_type, cm->seq_params.subsampling_x,
      cm->seq_params.subsampling_y)];
  const TX_SIZE tx_size_uv_horz = txsize_horz_map[av1_get_max_uv_txsize(
      mbmi->sb_type, cm->seq_params.subsampling_x,
      cm->seq_params.subsampling_y)];
  const int is_square_transform_size = mbmi->tx_size <= TX_64X64;
  int mask_id = 0;
  int offset = 0;
  const int half_ratio_tx_size_max32 =
      (mbmi->tx_size > TX_64X64) & (mbmi->tx_size <= TX_32X16);
  if (is_square_transform_size) {
    switch (mbmi->tx_size) {
      case TX_4X4: mask_id = mask_id_table_tx_4x4[bsize]; break;
      case TX_8X8:
        mask_id = mask_id_table_tx_8x8[bsize];
        offset = 19;
        break;
      case TX_16X16:
        mask_id = mask_id_table_tx_16x16[bsize];
        offset = 33;
        break;
      case TX_32X32:
        mask_id = mask_id_table_tx_32x32[bsize];
        offset = 42;
        break;
      case TX_64X64: mask_id = 46; break;
      default: assert(!is_square_transform_size); return;
    }
    mask_id += offset;
  } else if (half_ratio_tx_size_max32) {
    int tx_size_equal_block_size = bsize == txsize_to_bsize[mbmi->tx_size];
    mask_id =
        47 + 2 * (mbmi->tx_size - TX_4X8) + (tx_size_equal_block_size ? 0 : 1);
  } else if (mbmi->tx_size == TX_32X64) {
    mask_id = 59;
  } else if (mbmi->tx_size == TX_64X32) {
    mask_id = 60;
  } else {  // quarter ratio tx size
    mask_id = 61 + (mbmi->tx_size - TX_4X16);
  }
  row = mi_row % MI_SIZE_64X64;
  col = mi_col % MI_SIZE_64X64;
  shift = get_index_shift(col, row, &index);
  const int vert_shift = tx_size_y_vert <= TX_8X8 ? shift : col;
  for (int i = 0; i + index < 4; ++i) {
    // y vertical.
    lfm->tx_size_ver[0][tx_size_y_horz].bits[i + index] |=
        (left_mask_univariant_reordered[mask_id].bits[i] << vert_shift);
    // y horizontal.
    lfm->tx_size_hor[0][tx_size_y_vert].bits[i + index] |=
        (above_mask_univariant_reordered[mask_id].bits[i] << shift);
    // u/v vertical.
    lfm->tx_size_ver[1][tx_size_uv_horz].bits[i + index] |=
        (left_mask_univariant_reordered[mask_id].bits[i] << vert_shift);
    // u/v horizontal.
    lfm->tx_size_hor[1][tx_size_uv_vert].bits[i + index] |=
        (above_mask_univariant_reordered[mask_id].bits[i] << shift);
  }
}

static void store_bitmask_other_info(AV1_COMMON *cm, int mi_row, int mi_col,
                                     BLOCK_SIZE bsize, MB_MODE_INFO *mbmi,
                                     int is_horz_coding_block_border,
                                     int is_vert_coding_block_border) {
  int index;
  int shift;
  int row;
  LoopFilterMask *lfm = get_loop_filter_mask(cm, mi_row, mi_col);
  const int row_start = mi_row % MI_SIZE_64X64;
  const int col_start = mi_col % MI_SIZE_64X64;
  shift = get_index_shift(col_start, row_start, &index);
  if (is_horz_coding_block_border) {
    const int block_shift = shift + mi_size_wide[bsize];
    assert(block_shift <= 64);
    const uint64_t right_edge_shift =
        (block_shift == 64) ? 0xffffffffffffffff : ((uint64_t)1 << block_shift);
    const uint64_t left_edge_shift = (block_shift == 64)
                                         ? (((uint64_t)1 << shift) - 1)
                                         : ((uint64_t)1 << shift);
    assert(right_edge_shift > left_edge_shift);
    const uint64_t top_edge_mask = right_edge_shift - left_edge_shift;
    lfm->is_horz_border.bits[index] |= top_edge_mask;
  }
  if (is_vert_coding_block_border) {
    const int is_vert_border = mask_id_table_vert_border[bsize];
    const int vert_shift = block_size_high[bsize] <= 8 ? shift : col_start;
    for (int i = 0; i + index < 4; ++i) {
      lfm->is_vert_border.bits[i + index] |=
          (left_mask_univariant_reordered[is_vert_border].bits[i]
           << vert_shift);
    }
  }
  const int is_skip = mbmi->skip && is_inter_block(mbmi);
  if (is_skip) {
    const int is_skip_mask = mask_id_table_tx_4x4[bsize];
    for (int i = 0; i + index < 4; ++i) {
      lfm->skip.bits[i + index] |=
          (above_mask_univariant_reordered[is_skip_mask].bits[i] << shift);
    }
  }
  const uint8_t level_vert_y = get_filter_level(cm, &cm->lf_info, 0, 0, mbmi);
  const uint8_t level_horz_y = get_filter_level(cm, &cm->lf_info, 1, 0, mbmi);
  const uint8_t level_u = get_filter_level(cm, &cm->lf_info, 0, 1, mbmi);
  const uint8_t level_v = get_filter_level(cm, &cm->lf_info, 0, 2, mbmi);
  for (int r = mi_row; r < mi_row + mi_size_high[bsize]; r++) {
    index = 0;
    row = r % MI_SIZE_64X64;
    memset(&lfm->lfl_y_ver[row][col_start], level_vert_y,
           sizeof(uint8_t) * mi_size_wide[bsize]);
    memset(&lfm->lfl_y_hor[row][col_start], level_horz_y,
           sizeof(uint8_t) * mi_size_wide[bsize]);
    memset(&lfm->lfl_u_ver[row][col_start], level_u,
           sizeof(uint8_t) * mi_size_wide[bsize]);
    memset(&lfm->lfl_u_hor[row][col_start], level_u,
           sizeof(uint8_t) * mi_size_wide[bsize]);
    memset(&lfm->lfl_v_ver[row][col_start], level_v,
           sizeof(uint8_t) * mi_size_wide[bsize]);
    memset(&lfm->lfl_v_hor[row][col_start], level_v,
           sizeof(uint8_t) * mi_size_wide[bsize]);
  }
}
#endif

static void parse_decode_block(AV1Decoder *const pbi, ThreadData *const td,
                               int mi_row, int mi_col, aom_reader *r,
                               PARTITION_TYPE partition, BLOCK_SIZE bsize) {
  MACROBLOCKD *const xd = &td->xd;
  decode_mbmi_block(pbi, xd, mi_row, mi_col, r, partition, bsize, td);

  av1_visit_palette(pbi, xd, mi_row, mi_col, r, bsize,
                    av1_decode_palette_tokens);

  AV1_COMMON *cm = &pbi->common;
  const int num_planes = av1_num_planes(cm);
  MB_MODE_INFO *mbmi = xd->mi[0];
  int inter_block_tx = is_inter_block(mbmi) || is_intrabc_block(mbmi);
  if (cm->tx_mode == TX_MODE_SELECT && block_signals_txsize(bsize) &&
      !mbmi->skip && inter_block_tx && !xd->lossless[mbmi->segment_id]) {
    const TX_SIZE max_tx_size = max_txsize_rect_lookup[bsize];
    const int bh = tx_size_high_unit[max_tx_size];
    const int bw = tx_size_wide_unit[max_tx_size];
    const int width = block_size_wide[bsize] >> tx_size_wide_log2[0];
    const int height = block_size_high[bsize] >> tx_size_high_log2[0];

    for (int idy = 0; idy < height; idy += bh)
      for (int idx = 0; idx < width; idx += bw)
        read_tx_size_vartx(xd, mbmi, max_tx_size, 0,
#if LOOP_FILTER_BITMASK
                           cm, mi_row, mi_col, 1,
#endif
                           idy, idx, r);
  } else {
    mbmi->tx_size = read_tx_size(cm, xd, inter_block_tx, !mbmi->skip, r);
    if (inter_block_tx)
      memset(mbmi->inter_tx_size, mbmi->tx_size, sizeof(mbmi->inter_tx_size));
    set_txfm_ctxs(mbmi->tx_size, xd->n4_w, xd->n4_h,
                  mbmi->skip && is_inter_block(mbmi), xd);
#if LOOP_FILTER_BITMASK
    const int w = mi_size_wide[bsize];
    const int h = mi_size_high[bsize];
    if (w <= mi_size_wide[BLOCK_64X64] && h <= mi_size_high[BLOCK_64X64]) {
      store_bitmask_univariant_tx(cm, mi_row, mi_col, bsize, mbmi);
    } else {
      for (int row = 0; row < h; row += mi_size_high[BLOCK_64X64]) {
        for (int col = 0; col < w; col += mi_size_wide[BLOCK_64X64]) {
          store_bitmask_univariant_tx(cm, mi_row + row, mi_col + col,
                                      BLOCK_64X64, mbmi);
        }
      }
    }
#endif
  }
#if LOOP_FILTER_BITMASK
  const int w = mi_size_wide[bsize];
  const int h = mi_size_high[bsize];
  if (w <= mi_size_wide[BLOCK_64X64] && h <= mi_size_high[BLOCK_64X64]) {
    store_bitmask_other_info(cm, mi_row, mi_col, bsize, mbmi, 1, 1);
  } else {
    for (int row = 0; row < h; row += mi_size_high[BLOCK_64X64]) {
      for (int col = 0; col < w; col += mi_size_wide[BLOCK_64X64]) {
        store_bitmask_other_info(cm, mi_row + row, mi_col + col, BLOCK_64X64,
                                 mbmi, row == 0, col == 0);
      }
    }
  }
#endif

  if (cm->delta_q_info.delta_q_present_flag) {
    for (int i = 0; i < MAX_SEGMENTS; i++) {
      const int current_qindex =
          av1_get_qindex(&cm->seg, i, xd->current_qindex);
      for (int j = 0; j < num_planes; ++j) {
        const int dc_delta_q =
            j == 0 ? cm->y_dc_delta_q
                   : (j == 1 ? cm->u_dc_delta_q : cm->v_dc_delta_q);
        const int ac_delta_q =
            j == 0 ? 0 : (j == 1 ? cm->u_ac_delta_q : cm->v_ac_delta_q);
        xd->plane[j].seg_dequant_QTX[i][0] = av1_dc_quant_QTX(
            current_qindex, dc_delta_q, cm->seq_params.bit_depth);
        xd->plane[j].seg_dequant_QTX[i][1] = av1_ac_quant_QTX(
            current_qindex, ac_delta_q, cm->seq_params.bit_depth);
      }
    }
  }
  if (mbmi->skip) av1_reset_skip_context(xd, mi_row, mi_col, bsize, num_planes);

  decode_token_recon_block(pbi, td, mi_row, mi_col, r, bsize);
}

static void set_offsets_for_pred_and_recon(AV1Decoder *const pbi,
                                           ThreadData *const td, int mi_row,
                                           int mi_col, BLOCK_SIZE bsize) {
  AV1_COMMON *const cm = &pbi->common;
  MACROBLOCKD *const xd = &td->xd;
  const int bw = mi_size_wide[bsize];
  const int bh = mi_size_high[bsize];
  const int num_planes = av1_num_planes(cm);

  const int offset = mi_row * cm->mi_stride + mi_col;
  const TileInfo *const tile = &xd->tile;

  xd->mi = cm->mi_grid_visible + offset;
  xd->cfl.mi_row = mi_row;
  xd->cfl.mi_col = mi_col;

  set_plane_n4(xd, bw, bh, num_planes);

  // Distance of Mb to the various image edges. These are specified to 8th pel
  // as they are always compared to values that are in 1/8th pel units
  set_mi_row_col(xd, tile, mi_row, bh, mi_col, bw, cm->mi_rows, cm->mi_cols);

  av1_setup_dst_planes(xd->plane, bsize, &cm->cur_frame->buf, mi_row, mi_col, 0,
                       num_planes);
}

static void decode_block(AV1Decoder *const pbi, ThreadData *const td,
                         int mi_row, int mi_col, aom_reader *r,
                         PARTITION_TYPE partition, BLOCK_SIZE bsize) {
  (void)partition;
  set_offsets_for_pred_and_recon(pbi, td, mi_row, mi_col, bsize);
  decode_token_recon_block(pbi, td, mi_row, mi_col, r, bsize);
}

static PARTITION_TYPE read_partition(MACROBLOCKD *xd, int mi_row, int mi_col,
                                     aom_reader *r, int has_rows, int has_cols,
                                     BLOCK_SIZE bsize) {
  const int ctx = partition_plane_context(xd, mi_row, mi_col, bsize);
  FRAME_CONTEXT *ec_ctx = xd->tile_ctx;

  if (!has_rows && !has_cols) return PARTITION_SPLIT;

  assert(ctx >= 0);
  aom_cdf_prob *partition_cdf = ec_ctx->partition_cdf[ctx];
  if (has_rows && has_cols) {
    return (PARTITION_TYPE)aom_read_symbol(
        r, partition_cdf, partition_cdf_length(bsize), ACCT_STR);
  } else if (!has_rows && has_cols) {
    assert(bsize > BLOCK_8X8);
    aom_cdf_prob cdf[2];
    partition_gather_vert_alike(cdf, partition_cdf, bsize);
    assert(cdf[1] == AOM_ICDF(CDF_PROB_TOP));
    return aom_read_cdf(r, cdf, 2, ACCT_STR) ? PARTITION_SPLIT : PARTITION_HORZ;
  } else {
    assert(has_rows && !has_cols);
    assert(bsize > BLOCK_8X8);
    aom_cdf_prob cdf[2];
    partition_gather_horz_alike(cdf, partition_cdf, bsize);
    assert(cdf[1] == AOM_ICDF(CDF_PROB_TOP));
    return aom_read_cdf(r, cdf, 2, ACCT_STR) ? PARTITION_SPLIT : PARTITION_VERT;
  }
}

// TODO(slavarnway): eliminate bsize and subsize in future commits
static void decode_partition(AV1Decoder *const pbi, ThreadData *const td,
                             int mi_row, int mi_col, aom_reader *reader,
                             BLOCK_SIZE bsize, int parse_decode_flag) {
  AV1_COMMON *const cm = &pbi->common;
  MACROBLOCKD *const xd = &td->xd;
  const int bw = mi_size_wide[bsize];
  const int hbs = bw >> 1;
  PARTITION_TYPE partition;
  BLOCK_SIZE subsize;
  const int quarter_step = bw / 4;
  BLOCK_SIZE bsize2 = get_partition_subsize(bsize, PARTITION_SPLIT);
  const int has_rows = (mi_row + hbs) < cm->mi_rows;
  const int has_cols = (mi_col + hbs) < cm->mi_cols;

  if (mi_row >= cm->mi_rows || mi_col >= cm->mi_cols) return;

  // parse_decode_flag takes the following values :
  // 01 - do parse only
  // 10 - do decode only
  // 11 - do parse and decode
  static const block_visitor_fn_t block_visit[4] = {
    NULL, parse_decode_block, decode_block, parse_decode_block
  };

  if (parse_decode_flag & 1) {
    const int num_planes = av1_num_planes(cm);
    for (int plane = 0; plane < num_planes; ++plane) {
      int rcol0, rcol1, rrow0, rrow1;
      if (av1_loop_restoration_corners_in_sb(cm, plane, mi_row, mi_col, bsize,
                                             &rcol0, &rcol1, &rrow0, &rrow1)) {
        const int rstride = cm->rst_info[plane].horz_units_per_tile;
        for (int rrow = rrow0; rrow < rrow1; ++rrow) {
          for (int rcol = rcol0; rcol < rcol1; ++rcol) {
            const int runit_idx = rcol + rrow * rstride;
            loop_restoration_read_sb_coeffs(cm, xd, reader, plane, runit_idx);
          }
        }
      }
    }

    partition = (bsize < BLOCK_8X8) ? PARTITION_NONE
                                    : read_partition(xd, mi_row, mi_col, reader,
                                                     has_rows, has_cols, bsize);
  } else {
    partition = get_partition(cm, mi_row, mi_col, bsize);
  }
  subsize = get_partition_subsize(bsize, partition);

  // Check the bitstream is conformant: if there is subsampling on the
  // chroma planes, subsize must subsample to a valid block size.
  const struct macroblockd_plane *const pd_u = &xd->plane[1];
  if (get_plane_block_size(subsize, pd_u->subsampling_x, pd_u->subsampling_y) ==
      BLOCK_INVALID) {
    aom_internal_error(xd->error_info, AOM_CODEC_CORRUPT_FRAME,
                       "Block size %dx%d invalid with this subsampling mode",
                       block_size_wide[subsize], block_size_high[subsize]);
  }

#define DEC_BLOCK_STX_ARG
#define DEC_BLOCK_EPT_ARG partition,
#define DEC_BLOCK(db_r, db_c, db_subsize)                                  \
  block_visit[parse_decode_flag](pbi, td, DEC_BLOCK_STX_ARG(db_r), (db_c), \
                                 reader, DEC_BLOCK_EPT_ARG(db_subsize))
#define DEC_PARTITION(db_r, db_c, db_subsize)                        \
  decode_partition(pbi, td, DEC_BLOCK_STX_ARG(db_r), (db_c), reader, \
                   (db_subsize), parse_decode_flag)

  switch (partition) {
    case PARTITION_NONE: DEC_BLOCK(mi_row, mi_col, subsize); break;
    case PARTITION_HORZ:
      DEC_BLOCK(mi_row, mi_col, subsize);
      if (has_rows) DEC_BLOCK(mi_row + hbs, mi_col, subsize);
      break;
    case PARTITION_VERT:
      DEC_BLOCK(mi_row, mi_col, subsize);
      if (has_cols) DEC_BLOCK(mi_row, mi_col + hbs, subsize);
      break;
    case PARTITION_SPLIT:
      DEC_PARTITION(mi_row, mi_col, subsize);
      DEC_PARTITION(mi_row, mi_col + hbs, subsize);
      DEC_PARTITION(mi_row + hbs, mi_col, subsize);
      DEC_PARTITION(mi_row + hbs, mi_col + hbs, subsize);
      break;
    case PARTITION_HORZ_A:
      DEC_BLOCK(mi_row, mi_col, bsize2);
      DEC_BLOCK(mi_row, mi_col + hbs, bsize2);
      DEC_BLOCK(mi_row + hbs, mi_col, subsize);
      break;
    case PARTITION_HORZ_B:
      DEC_BLOCK(mi_row, mi_col, subsize);
      DEC_BLOCK(mi_row + hbs, mi_col, bsize2);
      DEC_BLOCK(mi_row + hbs, mi_col + hbs, bsize2);
      break;
    case PARTITION_VERT_A:
      DEC_BLOCK(mi_row, mi_col, bsize2);
      DEC_BLOCK(mi_row + hbs, mi_col, bsize2);
      DEC_BLOCK(mi_row, mi_col + hbs, subsize);
      break;
    case PARTITION_VERT_B:
      DEC_BLOCK(mi_row, mi_col, subsize);
      DEC_BLOCK(mi_row, mi_col + hbs, bsize2);
      DEC_BLOCK(mi_row + hbs, mi_col + hbs, bsize2);
      break;
    case PARTITION_HORZ_4:
      for (int i = 0; i < 4; ++i) {
        int this_mi_row = mi_row + i * quarter_step;
        if (i > 0 && this_mi_row >= cm->mi_rows) break;
        DEC_BLOCK(this_mi_row, mi_col, subsize);
      }
      break;
    case PARTITION_VERT_4:
      for (int i = 0; i < 4; ++i) {
        int this_mi_col = mi_col + i * quarter_step;
        if (i > 0 && this_mi_col >= cm->mi_cols) break;
        DEC_BLOCK(mi_row, this_mi_col, subsize);
      }
      break;
    default: assert(0 && "Invalid partition type");
  }

#undef DEC_PARTITION
#undef DEC_BLOCK
#undef DEC_BLOCK_EPT_ARG
#undef DEC_BLOCK_STX_ARG

  if (parse_decode_flag & 1)
    update_ext_partition_context(xd, mi_row, mi_col, subsize, bsize, partition);
}

static void setup_bool_decoder(const uint8_t *data, const uint8_t *data_end,
                               const size_t read_size,
                               struct aom_internal_error_info *error_info,
                               aom_reader *r, uint8_t allow_update_cdf) {
  // Validate the calculated partition length. If the buffer
  // described by the partition can't be fully read, then restrict
  // it to the portion that can be (for EC mode) or throw an error.
  if (!read_is_valid(data, read_size, data_end))
    aom_internal_error(error_info, AOM_CODEC_CORRUPT_FRAME,
                       "Truncated packet or corrupt tile length");

  if (aom_reader_init(r, data, read_size))
    aom_internal_error(error_info, AOM_CODEC_MEM_ERROR,
                       "Failed to allocate bool decoder %d", 1);

  r->allow_update_cdf = allow_update_cdf;
}

static void setup_segmentation(AV1_COMMON *const cm,
                               struct aom_read_bit_buffer *rb) {
  struct segmentation *const seg = &cm->seg;

  seg->update_map = 0;
  seg->update_data = 0;
  seg->temporal_update = 0;

  seg->enabled = aom_rb_read_bit(rb);
  if (!seg->enabled) {
    if (cm->cur_frame->seg_map)
      memset(cm->cur_frame->seg_map, 0, (cm->mi_rows * cm->mi_cols));

    memset(seg, 0, sizeof(*seg));
    segfeatures_copy(&cm->cur_frame->seg, seg);
    return;
  }
  if (cm->seg.enabled && cm->prev_frame &&
      (cm->mi_rows == cm->prev_frame->mi_rows) &&
      (cm->mi_cols == cm->prev_frame->mi_cols)) {
    cm->last_frame_seg_map = cm->prev_frame->seg_map;
  } else {
    cm->last_frame_seg_map = NULL;
  }
  // Read update flags
  if (cm->primary_ref_frame == PRIMARY_REF_NONE) {
    // These frames can't use previous frames, so must signal map + features
    seg->update_map = 1;
    seg->temporal_update = 0;
    seg->update_data = 1;
  } else {
    seg->update_map = aom_rb_read_bit(rb);
    if (seg->update_map) {
      seg->temporal_update = aom_rb_read_bit(rb);
    } else {
      seg->temporal_update = 0;
    }
    seg->update_data = aom_rb_read_bit(rb);
  }

  // Segmentation data update
  if (seg->update_data) {
    av1_clearall_segfeatures(seg);

    for (int i = 0; i < MAX_SEGMENTS; i++) {
      for (int j = 0; j < SEG_LVL_MAX; j++) {
        int data = 0;
        const int feature_enabled = aom_rb_read_bit(rb);
        if (feature_enabled) {
          av1_enable_segfeature(seg, i, j);

          const int data_max = av1_seg_feature_data_max(j);
          const int data_min = -data_max;
          const int ubits = get_unsigned_bits(data_max);

          if (av1_is_segfeature_signed(j)) {
            data = aom_rb_read_inv_signed_literal(rb, ubits);
          } else {
            data = aom_rb_read_literal(rb, ubits);
          }

          data = clamp(data, data_min, data_max);
        }
        av1_set_segdata(seg, i, j, data);
      }
    }
    calculate_segdata(seg);
  } else if (cm->prev_frame) {
    segfeatures_copy(seg, &cm->prev_frame->seg);
  }
  segfeatures_copy(&cm->cur_frame->seg, seg);
}

static void decode_restoration_mode(AV1_COMMON *cm,
                                    struct aom_read_bit_buffer *rb) {
  assert(!cm->all_lossless);
  const int num_planes = av1_num_planes(cm);
  if (cm->allow_intrabc) return;
  int all_none = 1, chroma_none = 1;
  for (int p = 0; p < num_planes; ++p) {
    RestorationInfo *rsi = &cm->rst_info[p];
    if (aom_rb_read_bit(rb)) {
      rsi->frame_restoration_type =
          aom_rb_read_bit(rb) ? RESTORE_SGRPROJ : RESTORE_WIENER;
    } else {
      rsi->frame_restoration_type =
          aom_rb_read_bit(rb) ? RESTORE_SWITCHABLE : RESTORE_NONE;
    }
    if (rsi->frame_restoration_type != RESTORE_NONE) {
      all_none = 0;
      chroma_none &= p == 0;
    }
  }
  if (!all_none) {
    assert(cm->seq_params.sb_size == BLOCK_64X64 ||
           cm->seq_params.sb_size == BLOCK_128X128);
    const int sb_size = cm->seq_params.sb_size == BLOCK_128X128 ? 128 : 64;

    for (int p = 0; p < num_planes; ++p)
      cm->rst_info[p].restoration_unit_size = sb_size;

    RestorationInfo *rsi = &cm->rst_info[0];

    if (sb_size == 64) {
      rsi->restoration_unit_size <<= aom_rb_read_bit(rb);
    }
    if (rsi->restoration_unit_size > 64) {
      rsi->restoration_unit_size <<= aom_rb_read_bit(rb);
    }
  } else {
    const int size = RESTORATION_UNITSIZE_MAX;
    for (int p = 0; p < num_planes; ++p)
      cm->rst_info[p].restoration_unit_size = size;
  }

  if (num_planes > 1) {
    int s = AOMMIN(cm->seq_params.subsampling_x, cm->seq_params.subsampling_y);
    if (s && !chroma_none) {
      cm->rst_info[1].restoration_unit_size =
          cm->rst_info[0].restoration_unit_size >> (aom_rb_read_bit(rb) * s);
    } else {
      cm->rst_info[1].restoration_unit_size =
          cm->rst_info[0].restoration_unit_size;
    }
    cm->rst_info[2].restoration_unit_size =
        cm->rst_info[1].restoration_unit_size;
  }
}

static void read_wiener_filter(int wiener_win, WienerInfo *wiener_info,
                               WienerInfo *ref_wiener_info, aom_reader *rb) {
  memset(wiener_info->vfilter, 0, sizeof(wiener_info->vfilter));
  memset(wiener_info->hfilter, 0, sizeof(wiener_info->hfilter));

  if (wiener_win == WIENER_WIN)
    wiener_info->vfilter[0] = wiener_info->vfilter[WIENER_WIN - 1] =
        aom_read_primitive_refsubexpfin(
            rb, WIENER_FILT_TAP0_MAXV - WIENER_FILT_TAP0_MINV + 1,
            WIENER_FILT_TAP0_SUBEXP_K,
            ref_wiener_info->vfilter[0] - WIENER_FILT_TAP0_MINV, ACCT_STR) +
        WIENER_FILT_TAP0_MINV;
  else
    wiener_info->vfilter[0] = wiener_info->vfilter[WIENER_WIN - 1] = 0;
  wiener_info->vfilter[1] = wiener_info->vfilter[WIENER_WIN - 2] =
      aom_read_primitive_refsubexpfin(
          rb, WIENER_FILT_TAP1_MAXV - WIENER_FILT_TAP1_MINV + 1,
          WIENER_FILT_TAP1_SUBEXP_K,
          ref_wiener_info->vfilter[1] - WIENER_FILT_TAP1_MINV, ACCT_STR) +
      WIENER_FILT_TAP1_MINV;
  wiener_info->vfilter[2] = wiener_info->vfilter[WIENER_WIN - 3] =
      aom_read_primitive_refsubexpfin(
          rb, WIENER_FILT_TAP2_MAXV - WIENER_FILT_TAP2_MINV + 1,
          WIENER_FILT_TAP2_SUBEXP_K,
          ref_wiener_info->vfilter[2] - WIENER_FILT_TAP2_MINV, ACCT_STR) +
      WIENER_FILT_TAP2_MINV;
  // The central element has an implicit +WIENER_FILT_STEP
  wiener_info->vfilter[WIENER_HALFWIN] =
      -2 * (wiener_info->vfilter[0] + wiener_info->vfilter[1] +
            wiener_info->vfilter[2]);

  if (wiener_win == WIENER_WIN)
    wiener_info->hfilter[0] = wiener_info->hfilter[WIENER_WIN - 1] =
        aom_read_primitive_refsubexpfin(
            rb, WIENER_FILT_TAP0_MAXV - WIENER_FILT_TAP0_MINV + 1,
            WIENER_FILT_TAP0_SUBEXP_K,
            ref_wiener_info->hfilter[0] - WIENER_FILT_TAP0_MINV, ACCT_STR) +
        WIENER_FILT_TAP0_MINV;
  else
    wiener_info->hfilter[0] = wiener_info->hfilter[WIENER_WIN - 1] = 0;
  wiener_info->hfilter[1] = wiener_info->hfilter[WIENER_WIN - 2] =
      aom_read_primitive_refsubexpfin(
          rb, WIENER_FILT_TAP1_MAXV - WIENER_FILT_TAP1_MINV + 1,
          WIENER_FILT_TAP1_SUBEXP_K,
          ref_wiener_info->hfilter[1] - WIENER_FILT_TAP1_MINV, ACCT_STR) +
      WIENER_FILT_TAP1_MINV;
  wiener_info->hfilter[2] = wiener_info->hfilter[WIENER_WIN - 3] =
      aom_read_primitive_refsubexpfin(
          rb, WIENER_FILT_TAP2_MAXV - WIENER_FILT_TAP2_MINV + 1,
          WIENER_FILT_TAP2_SUBEXP_K,
          ref_wiener_info->hfilter[2] - WIENER_FILT_TAP2_MINV, ACCT_STR) +
      WIENER_FILT_TAP2_MINV;
  // The central element has an implicit +WIENER_FILT_STEP
  wiener_info->hfilter[WIENER_HALFWIN] =
      -2 * (wiener_info->hfilter[0] + wiener_info->hfilter[1] +
            wiener_info->hfilter[2]);
  memcpy(ref_wiener_info, wiener_info, sizeof(*wiener_info));
}

static void read_sgrproj_filter(SgrprojInfo *sgrproj_info,
                                SgrprojInfo *ref_sgrproj_info, aom_reader *rb) {
  sgrproj_info->ep = aom_read_literal(rb, SGRPROJ_PARAMS_BITS, ACCT_STR);
  const sgr_params_type *params = &sgr_params[sgrproj_info->ep];

  if (params->r[0] == 0) {
    sgrproj_info->xqd[0] = 0;
    sgrproj_info->xqd[1] =
        aom_read_primitive_refsubexpfin(
            rb, SGRPROJ_PRJ_MAX1 - SGRPROJ_PRJ_MIN1 + 1, SGRPROJ_PRJ_SUBEXP_K,
            ref_sgrproj_info->xqd[1] - SGRPROJ_PRJ_MIN1, ACCT_STR) +
        SGRPROJ_PRJ_MIN1;
  } else if (params->r[1] == 0) {
    sgrproj_info->xqd[0] =
        aom_read_primitive_refsubexpfin(
            rb, SGRPROJ_PRJ_MAX0 - SGRPROJ_PRJ_MIN0 + 1, SGRPROJ_PRJ_SUBEXP_K,
            ref_sgrproj_info->xqd[0] - SGRPROJ_PRJ_MIN0, ACCT_STR) +
        SGRPROJ_PRJ_MIN0;
    sgrproj_info->xqd[1] = clamp((1 << SGRPROJ_PRJ_BITS) - sgrproj_info->xqd[0],
                                 SGRPROJ_PRJ_MIN1, SGRPROJ_PRJ_MAX1);
  } else {
    sgrproj_info->xqd[0] =
        aom_read_primitive_refsubexpfin(
            rb, SGRPROJ_PRJ_MAX0 - SGRPROJ_PRJ_MIN0 + 1, SGRPROJ_PRJ_SUBEXP_K,
            ref_sgrproj_info->xqd[0] - SGRPROJ_PRJ_MIN0, ACCT_STR) +
        SGRPROJ_PRJ_MIN0;
    sgrproj_info->xqd[1] =
        aom_read_primitive_refsubexpfin(
            rb, SGRPROJ_PRJ_MAX1 - SGRPROJ_PRJ_MIN1 + 1, SGRPROJ_PRJ_SUBEXP_K,
            ref_sgrproj_info->xqd[1] - SGRPROJ_PRJ_MIN1, ACCT_STR) +
        SGRPROJ_PRJ_MIN1;
  }

  memcpy(ref_sgrproj_info, sgrproj_info, sizeof(*sgrproj_info));
}

static void loop_restoration_read_sb_coeffs(const AV1_COMMON *const cm,
                                            MACROBLOCKD *xd,
                                            aom_reader *const r, int plane,
                                            int runit_idx) {
  const RestorationInfo *rsi = &cm->rst_info[plane];
  RestorationUnitInfo *rui = &rsi->unit_info[runit_idx];
  if (rsi->frame_restoration_type == RESTORE_NONE) return;

  assert(!cm->all_lossless);

  const int wiener_win = (plane > 0) ? WIENER_WIN_CHROMA : WIENER_WIN;
  WienerInfo *wiener_info = xd->wiener_info + plane;
  SgrprojInfo *sgrproj_info = xd->sgrproj_info + plane;

  if (rsi->frame_restoration_type == RESTORE_SWITCHABLE) {
    rui->restoration_type =
        aom_read_symbol(r, xd->tile_ctx->switchable_restore_cdf,
                        RESTORE_SWITCHABLE_TYPES, ACCT_STR);
    switch (rui->restoration_type) {
      case RESTORE_WIENER:
        read_wiener_filter(wiener_win, &rui->wiener_info, wiener_info, r);
        break;
      case RESTORE_SGRPROJ:
        read_sgrproj_filter(&rui->sgrproj_info, sgrproj_info, r);
        break;
      default: assert(rui->restoration_type == RESTORE_NONE); break;
    }
  } else if (rsi->frame_restoration_type == RESTORE_WIENER) {
    if (aom_read_symbol(r, xd->tile_ctx->wiener_restore_cdf, 2, ACCT_STR)) {
      rui->restoration_type = RESTORE_WIENER;
      read_wiener_filter(wiener_win, &rui->wiener_info, wiener_info, r);
    } else {
      rui->restoration_type = RESTORE_NONE;
    }
  } else if (rsi->frame_restoration_type == RESTORE_SGRPROJ) {
    if (aom_read_symbol(r, xd->tile_ctx->sgrproj_restore_cdf, 2, ACCT_STR)) {
      rui->restoration_type = RESTORE_SGRPROJ;
      read_sgrproj_filter(&rui->sgrproj_info, sgrproj_info, r);
    } else {
      rui->restoration_type = RESTORE_NONE;
    }
  }
}

static void setup_loopfilter(AV1_COMMON *cm, struct aom_read_bit_buffer *rb) {
  const int num_planes = av1_num_planes(cm);
  struct loopfilter *lf = &cm->lf;
  if (cm->allow_intrabc || cm->coded_lossless) {
    // write default deltas to frame buffer
    av1_set_default_ref_deltas(cm->cur_frame->ref_deltas);
    av1_set_default_mode_deltas(cm->cur_frame->mode_deltas);
    return;
  }
  assert(!cm->coded_lossless);
  if (cm->prev_frame) {
    // write deltas to frame buffer
    memcpy(lf->ref_deltas, cm->prev_frame->ref_deltas, REF_FRAMES);
    memcpy(lf->mode_deltas, cm->prev_frame->mode_deltas, MAX_MODE_LF_DELTAS);
  } else {
    av1_set_default_ref_deltas(lf->ref_deltas);
    av1_set_default_mode_deltas(lf->mode_deltas);
  }
  lf->filter_level[0] = aom_rb_read_literal(rb, 6);
  lf->filter_level[1] = aom_rb_read_literal(rb, 6);
  if (num_planes > 1) {
    if (lf->filter_level[0] || lf->filter_level[1]) {
      lf->filter_level_u = aom_rb_read_literal(rb, 6);
      lf->filter_level_v = aom_rb_read_literal(rb, 6);
    }
  }
  lf->sharpness_level = aom_rb_read_literal(rb, 3);

  // Read in loop filter deltas applied at the MB level based on mode or ref
  // frame.
  lf->mode_ref_delta_update = 0;

  lf->mode_ref_delta_enabled = aom_rb_read_bit(rb);
  if (lf->mode_ref_delta_enabled) {
    lf->mode_ref_delta_update = aom_rb_read_bit(rb);
    if (lf->mode_ref_delta_update) {
      for (int i = 0; i < REF_FRAMES; i++)
        if (aom_rb_read_bit(rb))
          lf->ref_deltas[i] = aom_rb_read_inv_signed_literal(rb, 6);

      for (int i = 0; i < MAX_MODE_LF_DELTAS; i++)
        if (aom_rb_read_bit(rb))
          lf->mode_deltas[i] = aom_rb_read_inv_signed_literal(rb, 6);
    }
  }

  // write deltas to frame buffer
  memcpy(cm->cur_frame->ref_deltas, lf->ref_deltas, REF_FRAMES);
  memcpy(cm->cur_frame->mode_deltas, lf->mode_deltas, MAX_MODE_LF_DELTAS);
}

static void setup_cdef(AV1_COMMON *cm, struct aom_read_bit_buffer *rb) {
  const int num_planes = av1_num_planes(cm);
  CdefInfo *const cdef_info = &cm->cdef_info;

  if (cm->allow_intrabc) return;
  cdef_info->cdef_pri_damping = aom_rb_read_literal(rb, 2) + 3;
  cdef_info->cdef_sec_damping = cdef_info->cdef_pri_damping;
  cdef_info->cdef_bits = aom_rb_read_literal(rb, 2);
  cdef_info->nb_cdef_strengths = 1 << cdef_info->cdef_bits;
  for (int i = 0; i < cdef_info->nb_cdef_strengths; i++) {
    cdef_info->cdef_strengths[i] = aom_rb_read_literal(rb, CDEF_STRENGTH_BITS);
    cdef_info->cdef_uv_strengths[i] =
        num_planes > 1 ? aom_rb_read_literal(rb, CDEF_STRENGTH_BITS) : 0;
  }
}

static INLINE int read_delta_q(struct aom_read_bit_buffer *rb) {
  return aom_rb_read_bit(rb) ? aom_rb_read_inv_signed_literal(rb, 6) : 0;
}

static void setup_quantization(AV1_COMMON *const cm,
                               struct aom_read_bit_buffer *rb) {
  const SequenceHeader *const seq_params = &cm->seq_params;
  const int num_planes = av1_num_planes(cm);
  cm->base_qindex = aom_rb_read_literal(rb, QINDEX_BITS);
  cm->y_dc_delta_q = read_delta_q(rb);
  if (num_planes > 1) {
    int diff_uv_delta = 0;
    if (seq_params->separate_uv_delta_q) diff_uv_delta = aom_rb_read_bit(rb);
    cm->u_dc_delta_q = read_delta_q(rb);
    cm->u_ac_delta_q = read_delta_q(rb);
    if (diff_uv_delta) {
      cm->v_dc_delta_q = read_delta_q(rb);
      cm->v_ac_delta_q = read_delta_q(rb);
    } else {
      cm->v_dc_delta_q = cm->u_dc_delta_q;
      cm->v_ac_delta_q = cm->u_ac_delta_q;
    }
  } else {
    cm->u_dc_delta_q = 0;
    cm->u_ac_delta_q = 0;
    cm->v_dc_delta_q = 0;
    cm->v_ac_delta_q = 0;
  }
  cm->using_qmatrix = aom_rb_read_bit(rb);
  if (cm->using_qmatrix) {
    cm->qm_y = aom_rb_read_literal(rb, QM_LEVEL_BITS);
    cm->qm_u = aom_rb_read_literal(rb, QM_LEVEL_BITS);
    if (!seq_params->separate_uv_delta_q)
      cm->qm_v = cm->qm_u;
    else
      cm->qm_v = aom_rb_read_literal(rb, QM_LEVEL_BITS);
  } else {
    cm->qm_y = 0;
    cm->qm_u = 0;
    cm->qm_v = 0;
  }
}

// Build y/uv dequant values based on segmentation.
static void setup_segmentation_dequant(AV1_COMMON *const cm,
                                       MACROBLOCKD *const xd) {
  const int bit_depth = cm->seq_params.bit_depth;
  const int using_qm = cm->using_qmatrix;
  // When segmentation is disabled, only the first value is used.  The
  // remaining are don't cares.
  const int max_segments = cm->seg.enabled ? MAX_SEGMENTS : 1;
  for (int i = 0; i < max_segments; ++i) {
    const int qindex = xd->qindex[i];
    cm->y_dequant_QTX[i][0] =
        av1_dc_quant_QTX(qindex, cm->y_dc_delta_q, bit_depth);
    cm->y_dequant_QTX[i][1] = av1_ac_quant_QTX(qindex, 0, bit_depth);
    cm->u_dequant_QTX[i][0] =
        av1_dc_quant_QTX(qindex, cm->u_dc_delta_q, bit_depth);
    cm->u_dequant_QTX[i][1] =
        av1_ac_quant_QTX(qindex, cm->u_ac_delta_q, bit_depth);
    cm->v_dequant_QTX[i][0] =
        av1_dc_quant_QTX(qindex, cm->v_dc_delta_q, bit_depth);
    cm->v_dequant_QTX[i][1] =
        av1_ac_quant_QTX(qindex, cm->v_ac_delta_q, bit_depth);
    const int lossless = xd->lossless[i];
    // NB: depends on base index so there is only 1 set per frame
    // No quant weighting when lossless or signalled not using QM
    int qmlevel = (lossless || using_qm == 0) ? NUM_QM_LEVELS - 1 : cm->qm_y;
    for (int j = 0; j < TX_SIZES_ALL; ++j) {
      cm->y_iqmatrix[i][j] = av1_iqmatrix(cm, qmlevel, AOM_PLANE_Y, j);
    }
    qmlevel = (lossless || using_qm == 0) ? NUM_QM_LEVELS - 1 : cm->qm_u;
    for (int j = 0; j < TX_SIZES_ALL; ++j) {
      cm->u_iqmatrix[i][j] = av1_iqmatrix(cm, qmlevel, AOM_PLANE_U, j);
    }
    qmlevel = (lossless || using_qm == 0) ? NUM_QM_LEVELS - 1 : cm->qm_v;
    for (int j = 0; j < TX_SIZES_ALL; ++j) {
      cm->v_iqmatrix[i][j] = av1_iqmatrix(cm, qmlevel, AOM_PLANE_V, j);
    }
  }
}

static InterpFilter read_frame_interp_filter(struct aom_read_bit_buffer *rb) {
  return aom_rb_read_bit(rb) ? SWITCHABLE
                             : aom_rb_read_literal(rb, LOG_SWITCHABLE_FILTERS);
}

static void setup_render_size(AV1_COMMON *cm, struct aom_read_bit_buffer *rb) {
  cm->render_width = cm->superres_upscaled_width;
  cm->render_height = cm->superres_upscaled_height;
  if (aom_rb_read_bit(rb))
    av1_read_frame_size(rb, 16, 16, &cm->render_width, &cm->render_height);
}

// TODO(afergs): make "struct aom_read_bit_buffer *const rb"?
static void setup_superres(AV1_COMMON *const cm, struct aom_read_bit_buffer *rb,
                           int *width, int *height) {
  cm->superres_upscaled_width = *width;
  cm->superres_upscaled_height = *height;

  const SequenceHeader *const seq_params = &cm->seq_params;
  if (!seq_params->enable_superres) return;

  if (aom_rb_read_bit(rb)) {
    cm->superres_scale_denominator =
        (uint8_t)aom_rb_read_literal(rb, SUPERRES_SCALE_BITS);
    cm->superres_scale_denominator += SUPERRES_SCALE_DENOMINATOR_MIN;
    // Don't edit cm->width or cm->height directly, or the buffers won't get
    // resized correctly
    av1_calculate_scaled_superres_size(width, height,
                                       cm->superres_scale_denominator);
  } else {
    // 1:1 scaling - ie. no scaling, scale not provided
    cm->superres_scale_denominator = SCALE_NUMERATOR;
  }
}

static void resize_context_buffers(AV1_COMMON *cm, int width, int height) {
#if CONFIG_SIZE_LIMIT
  if (width > DECODE_WIDTH_LIMIT || height > DECODE_HEIGHT_LIMIT)
    aom_internal_error(&cm->error, AOM_CODEC_CORRUPT_FRAME,
                       "Dimensions of %dx%d beyond allowed size of %dx%d.",
                       width, height, DECODE_WIDTH_LIMIT, DECODE_HEIGHT_LIMIT);
#endif
  if (cm->width != width || cm->height != height) {
    av1_set_mb_mi(cm, width, height);
    cm->width = width;
    cm->height = height;
  }

  cm->cur_frame->width = cm->width;
  cm->cur_frame->height = cm->height;
}

static void setup_buffer_pool(AV1_COMMON *cm) {
  BufferPool *const pool = cm->buffer_pool;
  const SequenceHeader *const seq_params = &cm->seq_params;

  lock_buffer_pool(pool);
  //if (aom_realloc_frame_buffer(
  //        &cm->cur_frame->buf, cm->width, cm->height, seq_params->subsampling_x,
  //        seq_params->subsampling_y, seq_params->use_highbitdepth,
  //        AOM_DEC_BORDER_IN_PIXELS, cm->byte_alignment,
  //        &cm->cur_frame->raw_frame_buffer, pool->get_fb_cb, pool->cb_priv)) {
  //  unlock_buffer_pool(pool);
  //  aom_internal_error(&cm->error, AOM_CODEC_MEM_ERROR,
  //                     "Failed to allocate frame buffer");
  //}
  if (av1_reallocate_frame_buffer(pool->cb_priv,
                                &cm->cur_frame->buf, 
                                cm->width, 
                                cm->height, 
                                cm->superres_upscaled_width,
                                seq_params->use_highbitdepth)) {
    unlock_buffer_pool(pool);
    aom_internal_error(&cm->error, AOM_CODEC_MEM_ERROR, "Failed to allocate frame buffer");
  }
  unlock_buffer_pool(pool);
  ensure_mv_buffer(cm->cur_frame, cm);

  cm->cur_frame->buf.bit_depth = (unsigned int)seq_params->bit_depth;
  cm->cur_frame->buf.color_primaries = seq_params->color_primaries;
  cm->cur_frame->buf.transfer_characteristics =
      seq_params->transfer_characteristics;
  cm->cur_frame->buf.matrix_coefficients = seq_params->matrix_coefficients;
  cm->cur_frame->buf.monochrome = seq_params->monochrome;
  cm->cur_frame->buf.chroma_sample_position =
      seq_params->chroma_sample_position;
  cm->cur_frame->buf.color_range = seq_params->color_range;
  cm->cur_frame->buf.render_width = cm->render_width;
  cm->cur_frame->buf.render_height = cm->render_height;
}

static void setup_frame_size(AV1_COMMON *cm, int frame_size_override_flag,
                             struct aom_read_bit_buffer *rb) {
  const SequenceHeader *const seq_params = &cm->seq_params;
  int width, height;

  if (frame_size_override_flag) {
    int num_bits_width = seq_params->num_bits_width;
    int num_bits_height = seq_params->num_bits_height;
    av1_read_frame_size(rb, num_bits_width, num_bits_height, &width, &height);
    if (width > seq_params->max_frame_width ||
        height > seq_params->max_frame_height) {
      aom_internal_error(&cm->error, AOM_CODEC_CORRUPT_FRAME,
                         "Frame dimensions are larger than the maximum values");
    }
  } else {
    width = seq_params->max_frame_width;
    height = seq_params->max_frame_height;
  }

  setup_superres(cm, rb, &width, &height);
  resize_context_buffers(cm, width, height);
  setup_render_size(cm, rb);
  setup_buffer_pool(cm);
}

static void setup_sb_size(SequenceHeader *seq_params,
                          struct aom_read_bit_buffer *rb) {
  set_sb_size(seq_params, aom_rb_read_bit(rb) ? BLOCK_128X128 : BLOCK_64X64);
}

static INLINE int valid_ref_frame_img_fmt(aom_bit_depth_t ref_bit_depth,
                                          int ref_xss, int ref_yss,
                                          aom_bit_depth_t this_bit_depth,
                                          int this_xss, int this_yss) {
  return ref_bit_depth == this_bit_depth && ref_xss == this_xss &&
         ref_yss == this_yss;
}

static void setup_frame_size_with_refs(AV1_COMMON *cm,
                                       struct aom_read_bit_buffer *rb) {
  int width, height;
  int found = 0;
  int has_valid_ref_frame = 0;
  for (int i = LAST_FRAME; i <= ALTREF_FRAME; ++i) {
    if (aom_rb_read_bit(rb)) {
      const RefCntBuffer *const ref_buf = get_ref_frame_buf(cm, i);
      // This will never be NULL in a normal stream, as streams are required to
      // have a shown keyframe before any inter frames, which would refresh all
      // the reference buffers. However, it might be null if we're starting in
      // the middle of a stream, and static analysis will error if we don't do
      // a null check here.
      if (ref_buf == NULL) {
        aom_internal_error(&cm->error, AOM_CODEC_CORRUPT_FRAME,
                           "Invalid condition: invalid reference buffer");
      } else {
        const YV12_BUFFER_CONFIG *const buf = &ref_buf->buf;
        width = buf->y_crop_width;
        height = buf->y_crop_height;
        cm->render_width = buf->render_width;
        cm->render_height = buf->render_height;
        setup_superres(cm, rb, &width, &height);
        resize_context_buffers(cm, width, height);
        found = 1;
        break;
      }
    }
  }

  const SequenceHeader *const seq_params = &cm->seq_params;
  if (!found) {
    int num_bits_width = seq_params->num_bits_width;
    int num_bits_height = seq_params->num_bits_height;

    av1_read_frame_size(rb, num_bits_width, num_bits_height, &width, &height);
    setup_superres(cm, rb, &width, &height);
    resize_context_buffers(cm, width, height);
    setup_render_size(cm, rb);
  }

  if (width <= 0 || height <= 0)
    aom_internal_error(&cm->error, AOM_CODEC_CORRUPT_FRAME,
                       "Invalid frame size");

  // Check to make sure at least one of frames that this frame references
  // has valid dimensions.
  for (int i = LAST_FRAME; i <= ALTREF_FRAME; ++i) {
    const RefCntBuffer *const ref_frame = get_ref_frame_buf(cm, i);
    has_valid_ref_frame |=
        valid_ref_frame_size(ref_frame->buf.y_crop_width,
                             ref_frame->buf.y_crop_height, width, height);
  }
  if (!has_valid_ref_frame)
    aom_internal_error(&cm->error, AOM_CODEC_CORRUPT_FRAME,
                       "Referenced frame has invalid size");
  for (int i = LAST_FRAME; i <= ALTREF_FRAME; ++i) {
    const RefCntBuffer *const ref_frame = get_ref_frame_buf(cm, i);
    if (!valid_ref_frame_img_fmt(
            ref_frame->buf.bit_depth, ref_frame->buf.subsampling_x,
            ref_frame->buf.subsampling_y, seq_params->bit_depth,
            seq_params->subsampling_x, seq_params->subsampling_y))
      aom_internal_error(&cm->error, AOM_CODEC_CORRUPT_FRAME,
                         "Referenced frame has incompatible color format");
  }
  setup_buffer_pool(cm);
}

// Same function as av1_read_uniform but reading from uncompresses header wb
static int rb_read_uniform(struct aom_read_bit_buffer *const rb, int n) {
  const int l = get_unsigned_bits(n);
  const int m = (1 << l) - n;
  const int v = aom_rb_read_literal(rb, l - 1);
  assert(l != 0);
  if (v < m)
    return v;
  else
    return (v << 1) - m + aom_rb_read_bit(rb);
}

static void read_tile_info_max_tile(AV1_COMMON *const cm,
                                    struct aom_read_bit_buffer *const rb) {
  int width_mi = ALIGN_POWER_OF_TWO(cm->mi_cols, cm->seq_params.mib_size_log2);
  int height_mi = ALIGN_POWER_OF_TWO(cm->mi_rows, cm->seq_params.mib_size_log2);
  int width_sb = width_mi >> cm->seq_params.mib_size_log2;
  int height_sb = height_mi >> cm->seq_params.mib_size_log2;

  av1_get_tile_limits(cm);
  cm->uniform_tile_spacing_flag = aom_rb_read_bit(rb);

  // Read tile columns
  if (cm->uniform_tile_spacing_flag) {
    cm->log2_tile_cols = cm->min_log2_tile_cols;
    while (cm->log2_tile_cols < cm->max_log2_tile_cols) {
      if (!aom_rb_read_bit(rb)) {
        break;
      }
      cm->log2_tile_cols++;
    }
  } else {
    int i;
    int start_sb;
    for (i = 0, start_sb = 0; width_sb > 0 && i < MAX_TILE_COLS; i++) {
      const int size_sb =
          1 + rb_read_uniform(rb, AOMMIN(width_sb, cm->max_tile_width_sb));
      cm->tile_col_start_sb[i] = start_sb;
      start_sb += size_sb;
      width_sb -= size_sb;
    }
    cm->tile_cols = i;
    cm->tile_col_start_sb[i] = start_sb + width_sb;
  }
  av1_calculate_tile_cols(cm);

  // Read tile rows
  if (cm->uniform_tile_spacing_flag) {
    cm->log2_tile_rows = cm->min_log2_tile_rows;
    while (cm->log2_tile_rows < cm->max_log2_tile_rows) {
      if (!aom_rb_read_bit(rb)) {
        break;
      }
      cm->log2_tile_rows++;
    }
  } else {
    int i;
    int start_sb;
    for (i = 0, start_sb = 0; height_sb > 0 && i < MAX_TILE_ROWS; i++) {
      const int size_sb =
          1 + rb_read_uniform(rb, AOMMIN(height_sb, cm->max_tile_height_sb));
      cm->tile_row_start_sb[i] = start_sb;
      start_sb += size_sb;
      height_sb -= size_sb;
    }
    cm->tile_rows = i;
    cm->tile_row_start_sb[i] = start_sb + height_sb;
  }
  av1_calculate_tile_rows(cm);
}

void av1_set_single_tile_decoding_mode(AV1_COMMON *const cm) {
  cm->single_tile_decoding = 0;
  if (cm->large_scale_tile) {
    struct loopfilter *lf = &cm->lf;
    RestorationInfo *const rst_info = cm->rst_info;
    const CdefInfo *const cdef_info = &cm->cdef_info;

    // Figure out single_tile_decoding by loopfilter_level.
    const int no_loopfilter = !(lf->filter_level[0] || lf->filter_level[1]);
    const int no_cdef = cdef_info->cdef_bits == 0 &&
                        cdef_info->cdef_strengths[0] == 0 &&
                        cdef_info->cdef_uv_strengths[0] == 0;
    const int no_restoration =
        rst_info[0].frame_restoration_type == RESTORE_NONE &&
        rst_info[1].frame_restoration_type == RESTORE_NONE &&
        rst_info[2].frame_restoration_type == RESTORE_NONE;
    assert(IMPLIES(cm->coded_lossless, no_loopfilter && no_cdef));
    assert(IMPLIES(cm->all_lossless, no_restoration));
    cm->single_tile_decoding = no_loopfilter && no_cdef && no_restoration;
  }
}

static void read_tile_info(AV1Decoder *const pbi,
                           struct aom_read_bit_buffer *const rb) {
  AV1_COMMON *const cm = &pbi->common;

  read_tile_info_max_tile(cm, rb);

  cm->context_update_tile_id = 0;
  if (cm->tile_rows * cm->tile_cols > 1) {
    // tile to use for cdf update
    cm->context_update_tile_id =
        aom_rb_read_literal(rb, cm->log2_tile_rows + cm->log2_tile_cols);
    if (cm->context_update_tile_id >= cm->tile_rows * cm->tile_cols) {
      aom_internal_error(&cm->error, AOM_CODEC_CORRUPT_FRAME,
                         "Invalid context_update_tile_id");
    }
    // tile size magnitude
    pbi->tile_size_bytes = aom_rb_read_literal(rb, 2) + 1;
  }
}

#if EXT_TILE_DEBUG
static void read_ext_tile_info(AV1Decoder *const pbi,
                               struct aom_read_bit_buffer *const rb) {
  AV1_COMMON *const cm = &pbi->common;

  // This information is stored as a separate byte.
  int mod = rb->bit_offset % CHAR_BIT;
  if (mod > 0) aom_rb_read_literal(rb, CHAR_BIT - mod);
  assert(rb->bit_offset % CHAR_BIT == 0);

  if (cm->tile_cols * cm->tile_rows > 1) {
    // Read the number of bytes used to store tile size
    pbi->tile_col_size_bytes = aom_rb_read_literal(rb, 2) + 1;
    pbi->tile_size_bytes = aom_rb_read_literal(rb, 2) + 1;
  }
}
#endif  // EXT_TILE_DEBUG

static size_t mem_get_varsize(const uint8_t *src, int sz) {
  switch (sz) {
    case 1: return src[0];
    case 2: return mem_get_le16(src);
    case 3: return mem_get_le24(src);
    case 4: return mem_get_le32(src);
    default: assert(0 && "Invalid size"); return -1;
  }
}

#if EXT_TILE_DEBUG
// Reads the next tile returning its size and adjusting '*data' accordingly
// based on 'is_last'. On return, '*data' is updated to point to the end of the
// raw tile buffer in the bit stream.
static void get_ls_tile_buffer(
    const uint8_t *const data_end, struct aom_internal_error_info *error_info,
    const uint8_t **data, TileBufferDec (*const tile_buffers)[MAX_TILE_COLS],
    int tile_size_bytes, int col, int row, int tile_copy_mode) {
  size_t size;

  size_t copy_size = 0;
  const uint8_t *copy_data = NULL;

  if (!read_is_valid(*data, tile_size_bytes, data_end))
    aom_internal_error(error_info, AOM_CODEC_CORRUPT_FRAME,
                       "Truncated packet or corrupt tile length");
  size = mem_get_varsize(*data, tile_size_bytes);

  // If tile_copy_mode = 1, then the top bit of the tile header indicates copy
  // mode.
  if (tile_copy_mode && (size >> (tile_size_bytes * 8 - 1)) == 1) {
    // The remaining bits in the top byte signal the row offset
    int offset = (size >> (tile_size_bytes - 1) * 8) & 0x7f;

    // Currently, only use tiles in same column as reference tiles.
    copy_data = tile_buffers[row - offset][col].data;
    copy_size = tile_buffers[row - offset][col].size;
    size = 0;
  } else {
    size += AV1_MIN_TILE_SIZE_BYTES;
  }

  *data += tile_size_bytes;

  if (size > (size_t)(data_end - *data))
    aom_internal_error(error_info, AOM_CODEC_CORRUPT_FRAME,
                       "Truncated packet or corrupt tile size");

  if (size > 0) {
    tile_buffers[row][col].data = *data;
    tile_buffers[row][col].size = size;
  } else {
    tile_buffers[row][col].data = copy_data;
    tile_buffers[row][col].size = copy_size;
  }

  *data += size;
}

// Returns the end of the last tile buffer
// (tile_buffers[cm->tile_rows - 1][cm->tile_cols - 1]).
static const uint8_t *get_ls_tile_buffers(
    AV1Decoder *pbi, const uint8_t *data, const uint8_t *data_end,
    TileBufferDec (*const tile_buffers)[MAX_TILE_COLS]) {
  AV1_COMMON *const cm = &pbi->common;
  const int tile_cols = cm->tile_cols;
  const int tile_rows = cm->tile_rows;
  const int have_tiles = tile_cols * tile_rows > 1;
  const uint8_t *raw_data_end;  // The end of the last tile buffer

  if (!have_tiles) {
    const size_t tile_size = data_end - data;
    tile_buffers[0][0].data = data;
    tile_buffers[0][0].size = tile_size;
    raw_data_end = NULL;
  } else {
    // We locate only the tile buffers that are required, which are the ones
    // specified by pbi->dec_tile_col and pbi->dec_tile_row. Also, we always
    // need the last (bottom right) tile buffer, as we need to know where the
    // end of the compressed frame buffer is for proper superframe decoding.

    const uint8_t *tile_col_data_end[MAX_TILE_COLS] = { NULL };
    const uint8_t *const data_start = data;

    const int dec_tile_row = AOMMIN(pbi->dec_tile_row, tile_rows);
    const int single_row = pbi->dec_tile_row >= 0;
    const int tile_rows_start = single_row ? dec_tile_row : 0;
    const int tile_rows_end = single_row ? tile_rows_start + 1 : tile_rows;
    const int dec_tile_col = AOMMIN(pbi->dec_tile_col, tile_cols);
    const int single_col = pbi->dec_tile_col >= 0;
    const int tile_cols_start = single_col ? dec_tile_col : 0;
    const int tile_cols_end = single_col ? tile_cols_start + 1 : tile_cols;

    const int tile_col_size_bytes = pbi->tile_col_size_bytes;
    const int tile_size_bytes = pbi->tile_size_bytes;
    int tile_width, tile_height;
    av1_get_uniform_tile_size(cm, &tile_width, &tile_height);
    const int tile_copy_mode =
        ((AOMMAX(tile_width, tile_height) << MI_SIZE_LOG2) <= 256) ? 1 : 0;
    // Read tile column sizes for all columns (we need the last tile buffer)
    for (int c = 0; c < tile_cols; ++c) {
      const int is_last = c == tile_cols - 1;
      size_t tile_col_size;

      if (!is_last) {
        tile_col_size = mem_get_varsize(data, tile_col_size_bytes);
        data += tile_col_size_bytes;
        tile_col_data_end[c] = data + tile_col_size;
      } else {
        tile_col_size = data_end - data;
        tile_col_data_end[c] = data_end;
      }
      data += tile_col_size;
    }

    data = data_start;

    // Read the required tile sizes.
    for (int c = tile_cols_start; c < tile_cols_end; ++c) {
      const int is_last = c == tile_cols - 1;

      if (c > 0) data = tile_col_data_end[c - 1];

      if (!is_last) data += tile_col_size_bytes;

      // Get the whole of the last column, otherwise stop at the required tile.
      for (int r = 0; r < (is_last ? tile_rows : tile_rows_end); ++r) {
        get_ls_tile_buffer(tile_col_data_end[c], &pbi->common.error, &data,
                           tile_buffers, tile_size_bytes, c, r, tile_copy_mode);
      }
    }

    // If we have not read the last column, then read it to get the last tile.
    if (tile_cols_end != tile_cols) {
      const int c = tile_cols - 1;

      data = tile_col_data_end[c - 1];

      for (int r = 0; r < tile_rows; ++r) {
        get_ls_tile_buffer(tile_col_data_end[c], &pbi->common.error, &data,
                           tile_buffers, tile_size_bytes, c, r, tile_copy_mode);
      }
    }
    raw_data_end = data;
  }
  return raw_data_end;
}
#endif  // EXT_TILE_DEBUG

// Reads the next tile returning its size and adjusting '*data' accordingly
// based on 'is_last'.
static void get_tile_buffer(const uint8_t *const data_end,
                            const int tile_size_bytes, int is_last,
                            struct aom_internal_error_info *error_info,
                            const uint8_t **data, TileBufferDec *const buf) {
  size_t size;

  if (!is_last) {
    if (!read_is_valid(*data, tile_size_bytes, data_end))
      aom_internal_error(error_info, AOM_CODEC_CORRUPT_FRAME,
                         "Truncated packet or corrupt tile length");

    size = mem_get_varsize(*data, tile_size_bytes) + AV1_MIN_TILE_SIZE_BYTES;
    *data += tile_size_bytes;

    if (size > (size_t)(data_end - *data))
      aom_internal_error(error_info, AOM_CODEC_CORRUPT_FRAME,
                         "Truncated packet or corrupt tile size");
  } else {
    size = data_end - *data;
  }

  buf->data = *data;
  buf->size = size;

  *data += size;
}

static void get_tile_buffers(AV1Decoder *pbi, const uint8_t *data,
                             const uint8_t *data_end,
                             TileBufferDec (*const tile_buffers)[MAX_TILE_COLS],
                             int start_tile, int end_tile) {
  AV1_COMMON *const cm = &pbi->common;
  const int tile_cols = cm->tile_cols;
  const int tile_rows = cm->tile_rows;
  int tc = 0;
  int first_tile_in_tg = 0;

  for (int r = 0; r < tile_rows; ++r) {
    for (int c = 0; c < tile_cols; ++c, ++tc) {
      TileBufferDec *const buf = &tile_buffers[r][c];

      const int is_last = (tc == end_tile);
      const size_t hdr_offset = 0;

      if (tc < start_tile || tc > end_tile) continue;

      if (data + hdr_offset >= data_end)
        aom_internal_error(&cm->error, AOM_CODEC_CORRUPT_FRAME,
                           "Data ended before all tiles were read.");
      first_tile_in_tg += tc == first_tile_in_tg ? pbi->tg_size : 0;
      data += hdr_offset;
      get_tile_buffer(data_end, pbi->tile_size_bytes, is_last,
                      &pbi->common.error, &data, buf);
    }
  }
}

static void set_cb_buffer(AV1Decoder *pbi, MACROBLOCKD *const xd,
                          CB_BUFFER *cb_buffer_base, const int num_planes,
                          int mi_row, int mi_col) {
  AV1_COMMON *const cm = &pbi->common;
  int mib_size_log2 = cm->seq_params.mib_size_log2;
  int stride = (cm->mi_cols >> mib_size_log2) + 1;
  int offset = (mi_row >> mib_size_log2) * stride + (mi_col >> mib_size_log2);
  CB_BUFFER *cb_buffer = cb_buffer_base + offset;
  xd->cb_buffer = cb_buffer;
  //cb_buffer->dqcoeff_ptr = 0;
  for (int plane = 0; plane < num_planes; ++plane) {
    xd->plane[plane].eob_data = cb_buffer->eob_data[plane];
    xd->cb_offset[plane] = 0;
    xd->txb_offset[plane] = 0;
  }
  xd->plane[0].color_index_map = cb_buffer->color_index_map[0];
  xd->plane[1].color_index_map = cb_buffer->color_index_map[1];
  xd->color_index_map_offset[0] = 0;
  xd->color_index_map_offset[1] = 0;
}

static void decoder_alloc_tile_data(AV1Decoder *pbi, const int n_tiles) {
  pbi->tile_data = pbi->tile_data_alloc;
  pbi->allocated_tiles = n_tiles;
  pbi->allocated_row_mt_sync_rows = 0;
}


// Deallocate decoder row synchronization related mutex and data
void av1_dec_row_mt_dealloc(AV1DecRowMTSync *dec_row_mt_sync) {
  if (dec_row_mt_sync != NULL) {
#if CONFIG_MULTITHREAD
    int i;
    if (dec_row_mt_sync->mutex_ != NULL) {
      for (i = 0; i < dec_row_mt_sync->allocated_sb_rows; ++i) {
        pthread_mutex_destroy(&dec_row_mt_sync->mutex_[i]);
      }
      aom_free(dec_row_mt_sync->mutex_);
    }
    if (dec_row_mt_sync->cond_ != NULL) {
      for (i = 0; i < dec_row_mt_sync->allocated_sb_rows; ++i) {
        pthread_cond_destroy(&dec_row_mt_sync->cond_[i]);
      }
      aom_free(dec_row_mt_sync->cond_);
    }
#endif  // CONFIG_MULTITHREAD
    aom_free(dec_row_mt_sync->cur_sb_col);

    // clear the structure as the source of this call may be a resize in which
    // case this call will be followed by an _alloc() which may fail.
    av1_zero(*dec_row_mt_sync);
  }
}


static int check_trailing_bits_after_symbol_coder(aom_reader *r) {
  if (aom_reader_has_overflowed(r)) return -1;

  uint32_t nb_bits = aom_reader_tell(r);
  uint32_t nb_bytes = (nb_bits + 7) >> 3;
  const uint8_t *p = aom_reader_find_begin(r) + nb_bytes;

  // aom_reader_tell() returns 1 for a newly initialized decoder, and the
  // return value only increases as values are decoded. So nb_bits > 0, and
  // thus p > p_begin. Therefore accessing p[-1] is safe.
  uint8_t last_byte = p[-1];
  uint8_t pattern = 128 >> ((nb_bits - 1) & 7);
  if ((last_byte & (2 * pattern - 1)) != pattern) return -1;

  // Make sure that all padding bytes are zero as required by the spec.
  const uint8_t *p_end = aom_reader_find_end(r);
  while (p < p_end) {
    if (*p != 0) return -1;
    p++;
  }
  return 0;
}

static void set_decode_func_pointers(ThreadData *td, int parse_decode_flag) {
  (void)parse_decode_flag;
  td->read_coeffs_tx_intra_block_visit = decode_block_void;
  td->predict_and_recon_intra_block_visit = decode_block_void;
  td->read_coeffs_tx_inter_block_visit = decode_block_void;
  td->inverse_tx_inter_block_visit = decode_block_void;
  td->predict_inter_block_visit = predict_inter_block_void;
  td->cfl_store_inter_block_visit = cfl_store_inter_block_void;

  td->read_coeffs_tx_intra_block_visit = read_coeffs_tx_intra_block;
  td->read_coeffs_tx_inter_block_visit = av1_read_coeffs_txb_facade;
}

static void decode_tile2(AV1Decoder *pbi, ThreadData *const td, int tile_row,
                         int tile_col, int flags) {
  TileInfo tile_info;
  int block = 0;
  AV1_COMMON *const cm = &pbi->common;
  const int num_planes = av1_num_planes(cm);

  static int tile_no = 0;
  ++tile_no;

  set_decode_func_pointers(td, 0x1);

  av1_tile_set_row(&tile_info, cm, tile_row);
  av1_tile_set_col(&tile_info, cm, tile_col);
  av1_zero_above_context(cm, &td->xd, tile_info.mi_col_start,
                         tile_info.mi_col_end, tile_row);
  av1_reset_loop_filter_delta(&td->xd, num_planes);
  av1_reset_loop_restoration(&td->xd, num_planes);

  av1_setup_macroblockd(pbi, cm, td, &tile_info);

  for (int mi_row = tile_info.mi_row_start; mi_row < tile_info.mi_row_end; mi_row += cm->seq_params.mib_size)
  {
    av1_zero_left_context(&td->xd);
    for (int mi_col = tile_info.mi_col_start; mi_col < tile_info.mi_col_end; mi_col += cm->seq_params.mib_size)
    {
      if (!td->ext_idct_buffer && td->tile_data->dq_buffer_ptr >= td->tile_data->dq_buffer_max) {
          av1_setup_ext_coef_buffer(pbi, cm, td);
          td->ext_idct_buffer = 1;
      }
      
      set_cb_buffer(pbi, &td->xd, &td->cb_buffer_base, num_planes, 0, 0);
      // Bit-stream parsing and decoding of the superblock
      decode_partition(pbi, td, mi_row, mi_col, td->bit_reader,
                       cm->seq_params.sb_size, 1);

      if (aom_reader_has_overflowed(td->bit_reader)) {
        aom_merge_corrupted_flag(&td->xd.corrupted, 1);
        return;
      }
    }
  }

  int corrupted =
      (check_trailing_bits_after_symbol_coder(td->bit_reader)) ? 1 : 0;
  aom_merge_corrupted_flag(&td->xd.corrupted, corrupted);

#if 1
  td->tile_data->mi_count = td->mi_count;
  if (td->tile_data2)
    td->tile_data2->mi_count = td->mi_count2;
  for (block = 0; block < td->mi_count; ++block) {
      MB_MODE_INFO *mi = &td->mi_pool[block];
      const int mi_col = mi->mi_col;
      const int mi_row = mi->mi_row;
      const int bsize = mi->sb_type;
      MACROBLOCKD *const xd = &td->xd;

      set_offsets_for_pred_and_recon(pbi, td, mi_row, mi_col, bsize);
      av1_mi_push_block(pbi, cm, xd);
  }
  for (block = 0; block < td->mi_count2; ++block) {
      MB_MODE_INFO *mi = &td->mi_pool2[block];
      const int mi_col = mi->mi_col;
      const int mi_row = mi->mi_row;
      const int bsize = mi->sb_type;
      MACROBLOCKD *const xd = &td->xd;
      set_offsets_for_pred_and_recon(pbi, td, mi_row, mi_col, bsize);
      av1_mi_push_block(pbi, cm, xd);
  }

#else
  set_decode_func_pointers(td, 0x2);

  //idct ;
  {
      for (int mi_row = tile_info.mi_row_start; mi_row < tile_info.mi_row_end; mi_row += cm->seq_params.mib_size)
      {
          for (int mi_col = tile_info.mi_col_start; mi_col < tile_info.mi_col_end; mi_col += cm->seq_params.mib_size)
          {
              set_cb_buffer(pbi, &td->xd, pbi->cb_buffer_base, num_planes, mi_row, mi_col);
              CB_BUFFER *cb = td->xd.cb_buffer;
              for (int blk = 0; blk < cb->tx_block_count; ++blk)
              {
                  tx_block_info * tx_block = &cb->tx_block_data[blk];
                  if (tx_block->eob)
                  {
                      const int bx = tx_block->x;
                      const int by = tx_block->y;
                      const int plane = tx_block->plane;
                      const struct macroblockd_plane *const pd = &td->xd.plane[plane];
                      const int dst_stride = ((cm->mi_cols + 31) & (~31)) * 4 >> (plane > 0);
                      int16_t * dst = pbi->res_buffer_base[plane] + bx + by * dst_stride;
                      tran_low_t *const dqcoeff = pd->dqcoeff_block + tx_block->dq_offset;
                      uint16_t scan_line = tx_block->max_scan_line;
                      uint16_t eob = tx_block->eob;
                      TxfmParam params;
                      params.tx_type = tx_block->tx_type;
                      params.tx_size = tx_block->tx_size;
                      params.eob = tx_block->eob;
                      params.lossless = tx_block->flags;
                      params.bd = td->xd.bd;
                      params.is_hbd = is_cur_buf_hbd(&td->xd);
                      params.tx_set_type = EXT_TX_SET_ALL16;


                      av1_highbd_inv_txfm_c(dqcoeff, dst, dst_stride, &params);
                    //  memset(dqcoeff, 0, (scan_line + 1) * sizeof(dqcoeff[0]));
                  }
              }
          }
      }

      for (int mi_row = tile_info.mi_row_start; mi_row < tile_info.mi_row_end; mi_row += cm->seq_params.mib_size)
      {
          for (int mi_col = tile_info.mi_col_start; mi_col < tile_info.mi_col_end; mi_col += cm->seq_params.mib_size)
          {
              set_cb_buffer(pbi, &td->xd, pbi->cb_buffer_base, num_planes, mi_row, mi_col);
              CB_BUFFER *cb = td->xd.cb_buffer;
              for (int blk = 0; blk < cb->tx_block_count; ++blk)
              {
                  tx_block_info * tx_block = &cb->tx_block_data[blk];
                  if (tx_block->eob)
                  {
                      const int plane = tx_block->plane;
                      const struct macroblockd_plane *const pd = &td->xd.plane[plane];
                      tran_low_t *const dqcoeff = pd->dqcoeff_block + tx_block->dq_offset;
                      uint16_t scan_line = tx_block->max_scan_line;
                      memset(dqcoeff, 0, (scan_line + 1) * sizeof(dqcoeff[0]));
                  }
              }
          }
      }
  }

  for (block = 0; block < td->mi_count; ++block) {
    MB_MODE_INFO *mi = &td->mi_pool[block];
    const int sb_mask = cm->seq_params.mib_size - 1;    
    const int mi_col = mi->mi_col;
    const int mi_row = mi->mi_row;
    const int bsize = mi->sb_type;
    MACROBLOCKD *const xd = &td->xd;

    if ((mi_col & sb_mask) == 0 && (mi_row & sb_mask) == 0)
      set_cb_buffer(pbi, &td->xd, pbi->cb_buffer_base, num_planes, mi_row, mi_col);

    set_offsets_for_pred_and_recon(pbi, td, mi_row, mi_col, bsize);

    av1_mi_push_block(cm, xd);

    {
      MB_MODE_INFO *mbmi = xd->mi[0];
      CFL_CTX *const cfl = &xd->cfl;
      cfl->is_chroma_reference = is_chroma_reference(
          mi_row, mi_col, bsize, cfl->subsampling_x, cfl->subsampling_y);



      if (!is_inter_block(mbmi)) {
        int row, col;
        assert(bsize == get_plane_block_size(bsize, xd->plane[0].subsampling_x, xd->plane[0].subsampling_y));
        const int max_blocks_wide = max_block_wide(xd, bsize, 0);
        const int max_blocks_high = max_block_high(xd, bsize, 0);
        const BLOCK_SIZE max_unit_bsize = BLOCK_64X64;
        int mu_blocks_wide =
            block_size_wide[max_unit_bsize] >> tx_size_wide_log2[0];
        int mu_blocks_high =
            block_size_high[max_unit_bsize] >> tx_size_high_log2[0];

        mu_blocks_wide = AOMMIN(max_blocks_wide, mu_blocks_wide);
        mu_blocks_high = AOMMIN(max_blocks_high, mu_blocks_high);

        for (row = 0; row < max_blocks_high; row += mu_blocks_high) {
          for (col = 0; col < max_blocks_wide; col += mu_blocks_wide) {
            for (int plane = 0; plane < num_planes; ++plane) {
              const struct macroblockd_plane *const pd = &xd->plane[plane];
              if (!is_chroma_reference(mi_row, mi_col, bsize, pd->subsampling_x,
                                       pd->subsampling_y))
                continue;

              const TX_SIZE tx_size = av1_get_tx_size(plane, xd);
              const int stepr = tx_size_high_unit[tx_size];
              const int stepc = tx_size_wide_unit[tx_size];

              const int unit_height = ROUND_POWER_OF_TWO(
                  AOMMIN(mu_blocks_high + row, max_blocks_high),
                  pd->subsampling_y);
              const int unit_width = ROUND_POWER_OF_TWO(
                  AOMMIN(mu_blocks_wide + col, max_blocks_wide),
                  pd->subsampling_x);

              for (int blk_row = row >> pd->subsampling_y; blk_row < unit_height; blk_row += stepr) {
                  for (int blk_col = col >> pd->subsampling_x; blk_col < unit_width; blk_col += stepc) {
                      int c, r;
                      av1_predict_intra_block_facade(cm, xd, plane, blk_col, blk_row, tx_size);
                      uint8_t *dst_ = &pd->dst.buf[(blk_row * pd->dst.stride + blk_col) << tx_size_wide_log2[0]];
                      const int new_res_stride = ((cm->mi_cols + 31) & (~31)) * 4 >> (plane > 0);
                      int res_x = (((mi_col * MI_SIZE) >> pd->subsampling_x) + (blk_col << tx_size_wide_log2[0])) & (~3);
                      int res_y = (((mi_row * MI_SIZE) >> pd->subsampling_y) + (blk_row << tx_size_wide_log2[0])) & (~3);

                      int16_t * new_res_ = pbi->res_buffer_base[plane] + res_x + res_y * new_res_stride;
                          //((mi_col * MI_SIZE) >> pd->subsampling_x) +
                          //((mi_row * MI_SIZE) >> pd->subsampling_y) * new_res_stride +
                          //((blk_col + blk_row * new_res_stride) << tx_size_wide_log2[0]);
                      uint8_t * dst = dst_;
                      int16_t * new_res = new_res_;
                      if (!mi->skip)
                      {
                          for (r = 0; r < (stepr << 2); ++r)
                          {
                              for (c = 0; c < (stepc << 2); ++c)
                              {
                                  dst[c] = clip_pixel_highbd((int)new_res[c] + (int)dst[c], xd->bd);
                              }
                              dst += pd->dst.stride;
                              new_res += new_res_stride;
                          }
                      }

                      if (plane == AOM_PLANE_Y && store_cfl_required(cm, xd)) {
                          cfl_store_tx(xd, blk_row, blk_col, tx_size, mbmi->sb_type);
                      }
                  }
              }
            }
          }
        }
      } else {
        predict_inter_block(cm, xd, mi_row, mi_col, bsize);
        
        //if (is_interintra_pred(mbmi))
        //    av1_inter_block_run(cm, xd);

        if (!mbmi->skip)
        {
            for (int plane = 0; plane < 3; ++plane)
            {
                const struct macroblockd_plane *const pd = &xd->plane[plane];
                if (is_chroma_reference(mi_row, mi_col, bsize, pd->subsampling_x, pd->subsampling_y))
                {
                    int x, y;
                    int w = MAX(4, block_size_wide[bsize] >> pd->subsampling_x);
                    int h = MAX(4, block_size_high[bsize] >> pd->subsampling_y);
                    uint8_t * dst = pd->dst.buf;
                    const int stride = pd->dst.stride;
                    const int new_res_stride = ((cm->mi_cols + 31) & (~31)) * 4 >> (plane > 0);
                    
                    int mi_col1 = mi_col;
                    int mi_row1 = mi_row;
                    if (pd->subsampling_y && (mi_row & 0x01) && (mi_size_high[mbmi->sb_type] == 1))
                        mi_row1 -= 1;
                    if (pd->subsampling_x && (mi_col & 0x01) && (mi_size_wide[mbmi->sb_type] == 1))
                        mi_col1 -= 1;
                    int16_t * new_res = pbi->res_buffer_base[plane] +
                        ((mi_col1 * MI_SIZE) >> pd->subsampling_x) +
                        ((mi_row1 * MI_SIZE) >> pd->subsampling_y) * new_res_stride;
                    for (y = 0; y < h; ++y)
                    {
                        for (x = 0; x < w; ++x)
                        {
                            dst[x] = clip_pixel_highbd((int)new_res[x] + (int)dst[x], xd->bd);
                        }
                        dst += stride;
                        new_res += new_res_stride;
                    }
                }
            }
        }
        cfl_store_inter_block(cm, xd);
      }

    //  if (/*!is_inter_block(mbmi) || is_intrabc_block(mbmi)*/ is_interintra_pred(mbmi))
    //  {
    //      int16_t * res[3];
    //      const int res_stride = ((cm->mi_cols + 31) & (~31)) * 4;
    //      res[0] = pbi->res_buffer_base[0] + (mi_col * MI_SIZE) + (mi_row * MI_SIZE) * res_stride;
    //      res[1] = pbi->res_buffer_base[1] + ((mi_col * MI_SIZE) >> 1) + ((mi_row * MI_SIZE) >> 1) * (res_stride >> 1);
    //      res[2] = pbi->res_buffer_base[2] + ((mi_col * MI_SIZE) >> 1) + ((mi_row * MI_SIZE) >> 1) * (res_stride >> 1);
    //      av1_intra_block_run(cm, xd, res, res_stride);
    //  }


      av1_visit_palette(pbi, xd, mi_row, mi_col, NULL, bsize,
                        set_color_index_map_offset);
    }
  }

//  {
//  static int tile_idx = 0;
//  ++tile_idx;
//    //      if (tile_idx >= 9)
//          {
//              int mib_size_log2 = cm->seq_params.mib_size_log2;
//              int stride = (cm->mi_cols >> mib_size_log2) + 1;
//              int y_stride = ((cm->mi_cols + 31) & (~31)) * 4;
//              av1_idct_run(&td->xd, pbi->cb_buffer_base, tile_info, stride, mib_size_log2, pbi->res_buffer_base, y_stride);
//              av1_prediction_run_all(cm, &tile_info);
//          }
//
//  }
#endif
}


static TileJobsDec *get_dec_job_info(AV1DecTileMT *tile_mt_info) {
  TileJobsDec *cur_job_info = NULL;
#if CONFIG_MULTITHREAD
  pthread_mutex_lock(tile_mt_info->job_mutex);

  if (tile_mt_info->jobs_dequeued < tile_mt_info->jobs_enqueued) {
    cur_job_info = tile_mt_info->job_queue + tile_mt_info->jobs_dequeued;
    tile_mt_info->jobs_dequeued++;
  }

  pthread_mutex_unlock(tile_mt_info->job_mutex);
#else
  (void)tile_mt_info;
#endif
  return cur_job_info;
}

static void tile_worker_hook_init(AV1Decoder *const pbi,
                                  DecWorkerData *const thread_data,
                                  const TileBufferDec *const tile_buffer,
                                  TileDataDec *const tile_data,
                                  uint8_t allow_update_cdf) {
  AV1_COMMON *cm = &pbi->common;
  ThreadData *const td = thread_data->td;
  int tile_row = tile_data->tile_info.tile_row;
  int tile_col = tile_data->tile_info.tile_col;

  td->bit_reader = tile_data->is_last? &pbi->bit_reader_end : &td->thread_bit_reader;
  av1_tile_init(&td->xd.tile, cm, tile_row, tile_col);
  td->xd.current_qindex = cm->base_qindex;
  setup_bool_decoder(tile_buffer->data, thread_data->data_end,
                     tile_buffer->size, &thread_data->error_info,
                     td->bit_reader, allow_update_cdf);
#if CONFIG_ACCOUNTING
  if (pbi->acct_enabled) {
    td->bit_reader->accounting = &pbi->accounting;
    td->bit_reader->accounting->last_tell_frac =
        aom_reader_tell_frac(td->bit_reader);
  } else {
    td->bit_reader->accounting = NULL;
  }
#endif
  av1_init_macroblockd(cm, &td->xd, NULL);
  td->xd.error_info = &thread_data->error_info;
  av1_init_above_context(cm, &td->xd, tile_row);

  // Initialise the tile context from the frame context
  td->xd.tile_ctx = tile_data->is_choosen_one ? &pbi->ctx_the_choosen_one : &td->tctx;
  *td->xd.tile_ctx = *cm->fc;
#if CONFIG_ACCOUNTING
  if (pbi->acct_enabled) {
    tile_data->bit_reader.accounting->last_tell_frac =
        aom_reader_tell_frac(&tile_data->bit_reader);
  }
#endif
}

static int tile_worker_hook(void *arg1, void *arg2) {
  DecWorkerData *const thread_data = (DecWorkerData *)arg1;
  AV1Decoder *const pbi = (AV1Decoder *)arg2;
  AV1_COMMON *cm = &pbi->common;
  ThreadData *const td = thread_data->td;
  uint8_t allow_update_cdf;
  // Timing only: tile decode time of this worker (0 while timing is off).
  const int64_t perf_t0 = av1_perf_ticks();

  // The jmp_buf is valid only for the duration of the function that calls
  // setjmp(). Therefore, this function must reset the 'setjmp' field to 0
  // before it returns.
  if (setjmp(thread_data->error_info.jmp)) {
    thread_data->error_info.setjmp = 0;
    thread_data->td->xd.corrupted = 1;
    av1_perf_cpu_add(pbi, AV1_PERF_CPU_ENTROPY_SUM, perf_t0);
    return 0;
  }
  thread_data->error_info.setjmp = 1;

  allow_update_cdf = cm->large_scale_tile ? 0 : 1;
  allow_update_cdf = allow_update_cdf && !cm->disable_cdf_update;

  assert(cm->tile_cols > 0);
  while (!td->xd.corrupted) {
    TileJobsDec *cur_job_info = get_dec_job_info(&pbi->tile_mt_info);

    if (cur_job_info != NULL) {
      const TileBufferDec *const tile_buffer = cur_job_info->tile_buffer;
      TileDataDec *const tile_data = cur_job_info->tile_data;
      tile_worker_hook_init(pbi, thread_data, tile_buffer, tile_data,
                            allow_update_cdf);
      // decode tile
      int tile_row = tile_data->tile_info.tile_row;
      int tile_col = tile_data->tile_info.tile_col;

      decode_tile2(pbi, td, tile_row, tile_col, 0);
    } else {
      break;
    }
  }
  thread_data->error_info.setjmp = 0;
  av1_perf_cpu_add(pbi, AV1_PERF_CPU_ENTROPY_SUM, perf_t0);
  return !td->xd.corrupted;
}

// sorts in descending order
static int compare_tile_buffers(const void *a, const void *b) {
  const TileJobsDec *const buf1 = (const TileJobsDec *)a;
  const TileJobsDec *const buf2 = (const TileJobsDec *)b;
  return (((int)buf2->tile_buffer->size) - ((int)buf1->tile_buffer->size));
}

static void enqueue_tile_jobs(AV1Decoder *pbi, AV1_COMMON *cm,
                              int tile_rows_start, int tile_rows_end,
                              int tile_cols_start, int tile_cols_end,
                              int start_tile, int end_tile) {
  AV1DecTileMT *tile_mt_info = &pbi->tile_mt_info;
  TileJobsDec *tile_job_queue = tile_mt_info->job_queue;
  tile_mt_info->jobs_enqueued = 0;
  tile_mt_info->jobs_dequeued = 0;

  for (int row = tile_rows_start; row < tile_rows_end; row++) {
    for (int col = tile_cols_start; col < tile_cols_end; col++) {
      if (row * cm->tile_cols + col < start_tile ||
          row * cm->tile_cols + col > end_tile)
        continue;
      tile_job_queue->tile_buffer = &pbi->tile_buffers[row][col];
      tile_job_queue->tile_data = pbi->tile_data + row * cm->tile_cols + col;
      tile_job_queue++;
      tile_mt_info->jobs_enqueued++;
    }
  }
}

static void alloc_dec_jobs(AV1DecTileMT *tile_mt_info, AV1_COMMON *cm,
                           int tile_rows, int tile_cols) {
  tile_mt_info->alloc_tile_rows = tile_rows;
  tile_mt_info->alloc_tile_cols = tile_cols;
  int num_tiles = tile_rows * tile_cols;
#if CONFIG_MULTITHREAD
  {
    for (int i = 0; i < num_tiles; i++) {
      pthread_mutex_init(&tile_mt_info->job_mutex[i], NULL);
    }
  }
#endif
}

void av1_free_mc_tmp_buf(ThreadData *thread_data) {
  int ref;
  if (thread_data)
  {
      for (ref = 0; ref < 2; ref++) {
          if (thread_data->mc_buf_use_highbd)
              aom_free(CONVERT_TO_SHORTPTR(thread_data->mc_buf[ref]));
          else
              aom_free(thread_data->mc_buf[ref]);
          thread_data->mc_buf[ref] = NULL;
      }
      thread_data->mc_buf_size = 0;
      thread_data->mc_buf_use_highbd = 0;

      aom_free(thread_data->tmp_conv_dst);
      thread_data->tmp_conv_dst = NULL;
      for (int i = 0; i < 2; ++i) {
          aom_free(thread_data->tmp_obmc_bufs[i]);
          thread_data->tmp_obmc_bufs[i] = NULL;
      }
  }
}


static void reset_dec_workers(AV1Decoder *pbi, AVxWorkerHook worker_hook,
                              int num_workers) {
  const AVxWorkerInterface *const winterface = aom_get_worker_interface();

  // Reset tile decoding hook
  for (int worker_idx = 0; worker_idx < num_workers; ++worker_idx) {
    AVxWorker *const worker = &pbi->tile_workers[worker_idx];
    DecWorkerData *const thread_data = pbi->thread_data + worker_idx;
    thread_data->td->xd = pbi->mb;
    thread_data->td->xd.corrupted = 0;
    thread_data->td->xd.mc_buf[0] = thread_data->td->mc_buf[0];
    thread_data->td->xd.mc_buf[1] = thread_data->td->mc_buf[1];
    thread_data->td->xd.tmp_conv_dst = thread_data->td->tmp_conv_dst;
    for (int j = 0; j < 2; ++j) {
      thread_data->td->xd.tmp_obmc_bufs[j] = thread_data->td->tmp_obmc_bufs[j];
    }
    thread_data->td->thread_id = worker_idx;

    winterface->sync(worker);
    
    worker->hook = worker_hook;
    worker->data1 = thread_data;
    worker->data2 = pbi;
  }
#if CONFIG_ACCOUNTING
  if (pbi->acct_enabled) {
    aom_accounting_reset(&pbi->accounting);
  }
#endif
}

static void launch_dec_workers(AV1Decoder *pbi, const uint8_t *data_end,
                               int num_workers) {
  const AVxWorkerInterface *const winterface = aom_get_worker_interface();

  for (int worker_idx = 0; worker_idx < num_workers; ++worker_idx) {
    AVxWorker *const worker = &pbi->tile_workers[worker_idx];
    DecWorkerData *const thread_data = (DecWorkerData *)worker->data1;

    thread_data->data_end = data_end;

    worker->had_error = 0;
    if (worker_idx == num_workers - 1) {
      winterface->execute(worker);
    } else {
      winterface->launch(worker);
    }
  }
}

static void sync_dec_workers(AV1Decoder *pbi, int num_workers) {
  const AVxWorkerInterface *const winterface = aom_get_worker_interface();
  int corrupted = 0;

  for (int worker_idx = num_workers; worker_idx > 0; --worker_idx) {
    AVxWorker *const worker = &pbi->tile_workers[worker_idx - 1];
    aom_merge_corrupted_flag(&corrupted, !winterface->sync(worker));
  }

  pbi->mb.corrupted = corrupted;
}

static void decode_mt_init(AV1Decoder *pbi) {
  AV1_COMMON *const cm = &pbi->common;
  const AVxWorkerInterface *const winterface = aom_get_worker_interface();
  int worker_idx;

  // Create workers and thread_data
  if (pbi->num_workers == 0) {
    const int num_threads = pbi->max_threads;
    memset(pbi->tile_workers, 0, sizeof(pbi->tile_workers));
    memset(pbi->thread_data, 0, sizeof(pbi->thread_data));

    for (worker_idx = 0; worker_idx < num_threads; ++worker_idx) {
      AVxWorker *const worker = &pbi->tile_workers[worker_idx];
      DecWorkerData *const thread_data = pbi->thread_data + worker_idx;
      ++pbi->num_workers;

      winterface->init(worker);
      worker->thread_name = "aom tile worker";
      if (worker_idx < num_threads - 1 && !winterface->reset(worker)) {
        aom_internal_error(&cm->error, AOM_CODEC_MEM_ERROR,
                           "Tile decoder thread creation failed");
      }
        // Main thread acts as a worker and uses the thread data in pbi
      thread_data->td = &pbi->td[worker_idx];
      thread_data->error_info.error_code = AOM_CODEC_OK;
      thread_data->error_info.setjmp = 0;
    }
  }
}

static void tile_mt_queue(AV1Decoder *pbi, int tile_cols, int tile_rows,
                          int tile_rows_start, int tile_rows_end,
                          int tile_cols_start, int tile_cols_end,
                          int start_tile, int end_tile) {
  AV1_COMMON *const cm = &pbi->common;
  if (pbi->tile_mt_info.alloc_tile_cols != tile_cols ||
      pbi->tile_mt_info.alloc_tile_rows != tile_rows) {
    av1_dealloc_dec_jobs(&pbi->tile_mt_info);
    alloc_dec_jobs(&pbi->tile_mt_info, cm, tile_rows, tile_cols);
  }
  enqueue_tile_jobs(pbi, cm, tile_rows_start, tile_rows_end, tile_cols_start,
                    tile_cols_end, start_tile, end_tile);
  qsort(pbi->tile_mt_info.job_queue, pbi->tile_mt_info.jobs_enqueued,
        sizeof(pbi->tile_mt_info.job_queue[0]), compare_tile_buffers);
}

static void dec_alloc_cb_buf(AV1Decoder *pbi) {
    pbi->cb_buffer_base = NULL;
//  AV1_COMMON *const cm = &pbi->common;
//  int size = ((cm->mi_rows >> cm->seq_params.mib_size_log2) + 1) *
//             ((cm->mi_cols >> cm->seq_params.mib_size_log2) + 1);
//
//  if (pbi->cb_buffer_alloc_size < size) {
//    av1_dec_free_cb_buf(pbi);
//    CHECK_MEM_ERROR(cm, pbi->cb_buffer_base,
//                    aom_memalign(32, sizeof(*pbi->cb_buffer_base) * size));
//    memset(pbi->cb_buffer_base, 0, sizeof(*pbi->cb_buffer_base) * size);
//    pbi->cb_buffer_alloc_size = size;
//  }
}

static const uint8_t *decode_tiles_mt(AV1Decoder *pbi, const uint8_t *data,
                                      const uint8_t *data_end, int start_tile,
                                      int end_tile) {
  AV1_COMMON *const cm = &pbi->common;
  const int tile_cols = cm->tile_cols;
  const int tile_rows = cm->tile_rows;
  const int n_tiles = tile_cols * tile_rows;
  TileBufferDec(*const tile_buffers)[MAX_TILE_COLS] = pbi->tile_buffers;
  const int dec_tile_row = AOMMIN(pbi->dec_tile_row, tile_rows);
  const int single_row = pbi->dec_tile_row >= 0;
  const int dec_tile_col = AOMMIN(pbi->dec_tile_col, tile_cols);
  const int single_col = pbi->dec_tile_col >= 0;
  int tile_rows_start;
  int tile_rows_end;
  int tile_cols_start;
  int tile_cols_end;
  int tile_count_tg;
  int num_workers;
  const uint8_t *raw_data_end = NULL;

  if (cm->large_scale_tile) {
    tile_rows_start = single_row ? dec_tile_row : 0;
    tile_rows_end = single_row ? dec_tile_row + 1 : tile_rows;
    tile_cols_start = single_col ? dec_tile_col : 0;
    tile_cols_end = single_col ? tile_cols_start + 1 : tile_cols;
  } else {
    tile_rows_start = 0;
    tile_rows_end = tile_rows;
    tile_cols_start = 0;
    tile_cols_end = tile_cols;
  }
  tile_count_tg = end_tile - start_tile + 1;
  num_workers = AOMMIN(pbi->max_threads, tile_count_tg);

  // No tiles to decode.
  if (tile_rows_end <= tile_rows_start || tile_cols_end <= tile_cols_start ||
      // First tile is larger than end_tile.
      tile_rows_start * tile_cols + tile_cols_start > end_tile ||
      // Last tile is smaller than start_tile.
      (tile_rows_end - 1) * tile_cols + tile_cols_end - 1 < start_tile)
    return data;

  assert(tile_rows <= MAX_TILE_ROWS);
  assert(tile_cols <= MAX_TILE_COLS);
  assert(tile_count_tg > 0);
  assert(num_workers > 0);
  assert(start_tile <= end_tile);
  assert(start_tile >= 0 && end_tile < n_tiles);

  //decode_mt_init(pbi);
  dec_alloc_cb_buf(pbi);

  // get tile size in tile group
#if EXT_TILE_DEBUG
  if (cm->large_scale_tile) assert(pbi->ext_tile_debug == 1);
  if (cm->large_scale_tile)
    raw_data_end = get_ls_tile_buffers(pbi, data, data_end, tile_buffers);
  else
#endif  // EXT_TILE_DEBUG
    get_tile_buffers(pbi, data, data_end, tile_buffers, start_tile, end_tile);

  if (pbi->tile_data == NULL || n_tiles != pbi->allocated_tiles) {
    decoder_alloc_tile_data(pbi, n_tiles);
  }

  for (int row = 0; row < tile_rows; row++) {
    for (int col = 0; col < tile_cols; col++) {
      TileDataDec *tile_data = pbi->tile_data + row * cm->tile_cols + col;
      av1_tile_init(&tile_data->tile_info, cm, row, col);
      tile_data->is_choosen_one = 0;
      tile_data->is_last = 0;
    }
  }
  if (cm->refresh_frame_context == REFRESH_FRAME_CONTEXT_BACKWARD)
      pbi->tile_data[cm->context_update_tile_id].is_choosen_one = 1;
  pbi->tile_data[end_tile].is_last = 1;


  tile_mt_queue(pbi, tile_cols, tile_rows, tile_rows_start, tile_rows_end,
                tile_cols_start, tile_cols_end, start_tile, end_tile);

  reset_dec_workers(pbi, tile_worker_hook, num_workers);
  launch_dec_workers(pbi, data_end, num_workers);
  sync_dec_workers(pbi, num_workers);

  if (pbi->mb.corrupted)
    aom_internal_error(&cm->error, AOM_CODEC_CORRUPT_FRAME,
                       "Failed to decode tile data");

  if (cm->large_scale_tile) {
    if (n_tiles == 1) {
      // Find the end of the single tile buffer
      return aom_reader_find_end(&pbi->bit_reader_end);
    }
    // Return the end of the last tile buffer
    return raw_data_end;
  }
  return aom_reader_find_end(&pbi->bit_reader_end);
}

static void error_handler(void *data) {
  AV1_COMMON *const cm = (AV1_COMMON *)data;
  aom_internal_error(&cm->error, AOM_CODEC_CORRUPT_FRAME, "Truncated packet");
}

// Reads the high_bitdepth and twelve_bit fields in color_config() and sets
// seq_params->bit_depth based on the values of those fields and
// seq_params->profile. Reports errors by calling rb->error_handler() or
// aom_internal_error().
static void read_bitdepth(struct aom_read_bit_buffer *rb,
                          SequenceHeader *seq_params,
                          struct aom_internal_error_info *error_info) {
  const int high_bitdepth = aom_rb_read_bit(rb);
  if (seq_params->profile == PROFILE_2 && high_bitdepth) {
    const int twelve_bit = aom_rb_read_bit(rb);
    seq_params->bit_depth = twelve_bit ? AOM_BITS_12 : AOM_BITS_10;
  } else if (seq_params->profile <= PROFILE_2) {
    seq_params->bit_depth = high_bitdepth ? AOM_BITS_10 : AOM_BITS_8;
  } else {
    aom_internal_error(error_info, AOM_CODEC_UNSUP_BITSTREAM,
                       "Unsupported profile/bit-depth combination");
  }
}

void av1_read_film_grain_params(AV1_COMMON *cm,
                                struct aom_read_bit_buffer *rb) {
  aom_film_grain_t *pars = &cm->film_grain_params;
  const SequenceHeader *const seq_params = &cm->seq_params;

  pars->apply_grain = aom_rb_read_bit(rb);
  if (!pars->apply_grain) {
    memset(pars, 0, sizeof(*pars));
    return;
  }

  pars->random_seed = aom_rb_read_literal(rb, 16);
  if (cm->current_frame.frame_type == INTER_FRAME)
    pars->update_parameters = aom_rb_read_bit(rb);
  else
    pars->update_parameters = 1;

  pars->bit_depth = seq_params->bit_depth;

  if (!pars->update_parameters) {
    // inherit parameters from a previous reference frame
    int film_grain_params_ref_idx = aom_rb_read_literal(rb, 3);
    RefCntBuffer *const buf = cm->ref_frame_map[film_grain_params_ref_idx];
    if (buf == NULL) {
      aom_internal_error(&cm->error, AOM_CODEC_UNSUP_BITSTREAM,
                         "Invalid Film grain reference idx");
    }
    if (!buf->film_grain_params_present) {
      aom_internal_error(&cm->error, AOM_CODEC_UNSUP_BITSTREAM,
                         "Film grain reference parameters not available");
    }
    uint16_t random_seed = pars->random_seed;
    *pars = buf->film_grain_params;   // inherit paramaters
    pars->random_seed = random_seed;  // with new random seed
    return;
  }

  // Scaling functions parameters
  pars->num_y_points = aom_rb_read_literal(rb, 4);  // max 14
  if (pars->num_y_points > 14)
    aom_internal_error(&cm->error, AOM_CODEC_UNSUP_BITSTREAM,
                       "Number of points for film grain luma scaling function "
                       "exceeds the maximum value.");
  for (int i = 0; i < pars->num_y_points; i++) {
    pars->scaling_points_y[i][0] = aom_rb_read_literal(rb, 8);
    if (i && pars->scaling_points_y[i - 1][0] >= pars->scaling_points_y[i][0])
      aom_internal_error(&cm->error, AOM_CODEC_UNSUP_BITSTREAM,
                         "First coordinate of the scaling function points "
                         "shall be increasing.");
    pars->scaling_points_y[i][1] = aom_rb_read_literal(rb, 8);
  }

  if (!seq_params->monochrome)
    pars->chroma_scaling_from_luma = aom_rb_read_bit(rb);
  else
    pars->chroma_scaling_from_luma = 0;

  if (seq_params->monochrome || pars->chroma_scaling_from_luma ||
      ((seq_params->subsampling_x == 1) && (seq_params->subsampling_y == 1) &&
       (pars->num_y_points == 0))) {
    pars->num_cb_points = 0;
    pars->num_cr_points = 0;
  } else {
    pars->num_cb_points = aom_rb_read_literal(rb, 4);  // max 10
    if (pars->num_cb_points > 10)
      aom_internal_error(&cm->error, AOM_CODEC_UNSUP_BITSTREAM,
                         "Number of points for film grain cb scaling function "
                         "exceeds the maximum value.");
    for (int i = 0; i < pars->num_cb_points; i++) {
      pars->scaling_points_cb[i][0] = aom_rb_read_literal(rb, 8);
      if (i &&
          pars->scaling_points_cb[i - 1][0] >= pars->scaling_points_cb[i][0])
        aom_internal_error(&cm->error, AOM_CODEC_UNSUP_BITSTREAM,
                           "First coordinate of the scaling function points "
                           "shall be increasing.");
      pars->scaling_points_cb[i][1] = aom_rb_read_literal(rb, 8);
    }

    pars->num_cr_points = aom_rb_read_literal(rb, 4);  // max 10
    if (pars->num_cr_points > 10)
      aom_internal_error(&cm->error, AOM_CODEC_UNSUP_BITSTREAM,
                         "Number of points for film grain cr scaling function "
                         "exceeds the maximum value.");
    for (int i = 0; i < pars->num_cr_points; i++) {
      pars->scaling_points_cr[i][0] = aom_rb_read_literal(rb, 8);
      if (i &&
          pars->scaling_points_cr[i - 1][0] >= pars->scaling_points_cr[i][0])
        aom_internal_error(&cm->error, AOM_CODEC_UNSUP_BITSTREAM,
                           "First coordinate of the scaling function points "
                           "shall be increasing.");
      pars->scaling_points_cr[i][1] = aom_rb_read_literal(rb, 8);
    }

    if ((seq_params->subsampling_x == 1) && (seq_params->subsampling_y == 1) &&
        (((pars->num_cb_points == 0) && (pars->num_cr_points != 0)) ||
         ((pars->num_cb_points != 0) && (pars->num_cr_points == 0))))
      aom_internal_error(&cm->error, AOM_CODEC_UNSUP_BITSTREAM,
                         "In YCbCr 4:2:0, film grain shall be applied "
                         "to both chroma components or neither.");
  }

  pars->scaling_shift = aom_rb_read_literal(rb, 2) + 8;  // 8 + value

  // AR coefficients
  // Only sent if the corresponsing scaling function has
  // more than 0 points

  pars->ar_coeff_lag = aom_rb_read_literal(rb, 2);

  int num_pos_luma = 2 * pars->ar_coeff_lag * (pars->ar_coeff_lag + 1);
  int num_pos_chroma = num_pos_luma;
  if (pars->num_y_points > 0) ++num_pos_chroma;

  if (pars->num_y_points)
    for (int i = 0; i < num_pos_luma; i++)
      pars->ar_coeffs_y[i] = aom_rb_read_literal(rb, 8) - 128;

  if (pars->num_cb_points || pars->chroma_scaling_from_luma)
    for (int i = 0; i < num_pos_chroma; i++)
      pars->ar_coeffs_cb[i] = aom_rb_read_literal(rb, 8) - 128;

  if (pars->num_cr_points || pars->chroma_scaling_from_luma)
    for (int i = 0; i < num_pos_chroma; i++)
      pars->ar_coeffs_cr[i] = aom_rb_read_literal(rb, 8) - 128;

  pars->ar_coeff_shift = aom_rb_read_literal(rb, 2) + 6;  // 6 + value

  pars->grain_scale_shift = aom_rb_read_literal(rb, 2);

  if (pars->num_cb_points) {
    pars->cb_mult = aom_rb_read_literal(rb, 8);
    pars->cb_luma_mult = aom_rb_read_literal(rb, 8);
    pars->cb_offset = aom_rb_read_literal(rb, 9);
  }

  if (pars->num_cr_points) {
    pars->cr_mult = aom_rb_read_literal(rb, 8);
    pars->cr_luma_mult = aom_rb_read_literal(rb, 8);
    pars->cr_offset = aom_rb_read_literal(rb, 9);
  }

  pars->overlap_flag = aom_rb_read_bit(rb);

  pars->clip_to_restricted_range = aom_rb_read_bit(rb);
}

static void read_film_grain(AV1_COMMON *cm, struct aom_read_bit_buffer *rb) {
  if (cm->seq_params.film_grain_params_present &&
      (cm->show_frame || cm->showable_frame)) {
    av1_read_film_grain_params(cm, rb);
  } else {
    memset(&cm->film_grain_params, 0, sizeof(cm->film_grain_params));
  }
  cm->film_grain_params.bit_depth = cm->seq_params.bit_depth;
  memcpy(&cm->cur_frame->film_grain_params, &cm->film_grain_params,
         sizeof(aom_film_grain_t));
}

void av1_read_color_config(struct aom_read_bit_buffer *rb,
                           int allow_lowbitdepth, SequenceHeader *seq_params,
                           struct aom_internal_error_info *error_info) {
  read_bitdepth(rb, seq_params, error_info);

  seq_params->use_highbitdepth =
      seq_params->bit_depth > AOM_BITS_8 || !allow_lowbitdepth;

  // monochrome bit (not needed for PROFILE_1)
  const int is_monochrome =
      seq_params->profile != PROFILE_1 ? aom_rb_read_bit(rb) : 0;
  seq_params->monochrome = is_monochrome;
  int color_description_present_flag = aom_rb_read_bit(rb);
  if (color_description_present_flag) {
    seq_params->color_primaries = aom_rb_read_literal(rb, 8);
    seq_params->transfer_characteristics = aom_rb_read_literal(rb, 8);
    seq_params->matrix_coefficients = aom_rb_read_literal(rb, 8);
  } else {
    seq_params->color_primaries = AOM_CICP_CP_UNSPECIFIED;
    seq_params->transfer_characteristics = AOM_CICP_TC_UNSPECIFIED;
    seq_params->matrix_coefficients = AOM_CICP_MC_UNSPECIFIED;
  }
  if (is_monochrome) {
    // [16,235] (including xvycc) vs [0,255] range
    seq_params->color_range = aom_rb_read_bit(rb);
    seq_params->subsampling_y = seq_params->subsampling_x = 1;
    seq_params->chroma_sample_position = AOM_CSP_UNKNOWN;
    seq_params->separate_uv_delta_q = 0;
    return;
  }
  if (seq_params->color_primaries == AOM_CICP_CP_BT_709 &&
      seq_params->transfer_characteristics == AOM_CICP_TC_SRGB &&
      seq_params->matrix_coefficients == AOM_CICP_MC_IDENTITY) {
    seq_params->subsampling_y = seq_params->subsampling_x = 0;
    seq_params->color_range = 1;  // assume full color-range
    if (!(seq_params->profile == PROFILE_1 ||
          (seq_params->profile == PROFILE_2 &&
           seq_params->bit_depth == AOM_BITS_12))) {
      aom_internal_error(
          error_info, AOM_CODEC_UNSUP_BITSTREAM,
          "sRGB colorspace not compatible with specified profile");
    }
  } else {
    // [16,235] (including xvycc) vs [0,255] range
    seq_params->color_range = aom_rb_read_bit(rb);
    if (seq_params->profile == PROFILE_0) {
      // 420 only
      seq_params->subsampling_x = seq_params->subsampling_y = 1;
    } else if (seq_params->profile == PROFILE_1) {
      // 444 only
      seq_params->subsampling_x = seq_params->subsampling_y = 0;
    } else {
      assert(seq_params->profile == PROFILE_2);
      if (seq_params->bit_depth == AOM_BITS_12) {
        seq_params->subsampling_x = aom_rb_read_bit(rb);
        if (seq_params->subsampling_x)
          seq_params->subsampling_y = aom_rb_read_bit(rb);  // 422 or 420
        else
          seq_params->subsampling_y = 0;  // 444
      } else {
        // 422
        seq_params->subsampling_x = 1;
        seq_params->subsampling_y = 0;
      }
    }
    if (seq_params->matrix_coefficients == AOM_CICP_MC_IDENTITY &&
        (seq_params->subsampling_x || seq_params->subsampling_y)) {
      aom_internal_error(
          error_info, AOM_CODEC_UNSUP_BITSTREAM,
          "Identity CICP Matrix incompatible with non 4:4:4 color sampling");
    }
    if (seq_params->subsampling_x && seq_params->subsampling_y) {
      seq_params->chroma_sample_position = aom_rb_read_literal(rb, 2);
    }
  }
  seq_params->separate_uv_delta_q = aom_rb_read_bit(rb);
}

void av1_read_timing_info_header(AV1_COMMON *cm,
                                 struct aom_read_bit_buffer *rb) {
  cm->timing_info.num_units_in_display_tick = aom_rb_read_unsigned_literal(
      rb, 32);  // Number of units in a display tick
  cm->timing_info.time_scale =
      aom_rb_read_unsigned_literal(rb, 32);  // Time scale
  if (cm->timing_info.num_units_in_display_tick == 0 ||
      cm->timing_info.time_scale == 0) {
    aom_internal_error(
        &cm->error, AOM_CODEC_UNSUP_BITSTREAM,
        "num_units_in_display_tick and time_scale must be greater than 0.");
  }
  cm->timing_info.equal_picture_interval =
      aom_rb_read_bit(rb);  // Equal picture interval bit
  if (cm->timing_info.equal_picture_interval) {
    cm->timing_info.num_ticks_per_picture =
        aom_rb_read_uvlc(rb) + 1;  // ticks per picture
    if (cm->timing_info.num_ticks_per_picture == 0) {
      aom_internal_error(
          &cm->error, AOM_CODEC_UNSUP_BITSTREAM,
          "num_ticks_per_picture_minus_1 cannot be (1 << 32) − 1.");
    }
  }
}

void av1_read_decoder_model_info(AV1_COMMON *cm,
                                 struct aom_read_bit_buffer *rb) {
  cm->buffer_model.encoder_decoder_buffer_delay_length =
      aom_rb_read_literal(rb, 5) + 1;
  cm->buffer_model.num_units_in_decoding_tick = aom_rb_read_unsigned_literal(
      rb, 32);  // Number of units in a decoding tick
  cm->buffer_model.buffer_removal_time_length = aom_rb_read_literal(rb, 5) + 1;
  cm->buffer_model.frame_presentation_time_length =
      aom_rb_read_literal(rb, 5) + 1;
}

void av1_read_op_parameters_info(AV1_COMMON *const cm,
                                 struct aom_read_bit_buffer *rb, int op_num) {
  // The cm->op_params array has MAX_NUM_OPERATING_POINTS + 1 elements.
  if (op_num > MAX_NUM_OPERATING_POINTS) {
    aom_internal_error(&cm->error, AOM_CODEC_UNSUP_BITSTREAM,
                       "AV1 does not support %d decoder model operating points",
                       op_num + 1);
  }

  cm->op_params[op_num].decoder_buffer_delay = aom_rb_read_unsigned_literal(
      rb, cm->buffer_model.encoder_decoder_buffer_delay_length);

  cm->op_params[op_num].encoder_buffer_delay = aom_rb_read_unsigned_literal(
      rb, cm->buffer_model.encoder_decoder_buffer_delay_length);

  cm->op_params[op_num].low_delay_mode_flag = aom_rb_read_bit(rb);
}

static void av1_read_temporal_point_info(AV1_COMMON *const cm,
                                         struct aom_read_bit_buffer *rb) {
  cm->frame_presentation_time = aom_rb_read_unsigned_literal(
      rb, cm->buffer_model.frame_presentation_time_length);
}

void av1_read_sequence_header(AV1_COMMON *cm, struct aom_read_bit_buffer *rb,
                              SequenceHeader *seq_params) {
  const int num_bits_width = aom_rb_read_literal(rb, 4) + 1;
  const int num_bits_height = aom_rb_read_literal(rb, 4) + 1;
  const int max_frame_width = aom_rb_read_literal(rb, num_bits_width) + 1;
  const int max_frame_height = aom_rb_read_literal(rb, num_bits_height) + 1;

  seq_params->num_bits_width = num_bits_width;
  seq_params->num_bits_height = num_bits_height;
  seq_params->max_frame_width = max_frame_width;
  seq_params->max_frame_height = max_frame_height;

  if (seq_params->reduced_still_picture_hdr) {
    seq_params->frame_id_numbers_present_flag = 0;
  } else {
    seq_params->frame_id_numbers_present_flag = aom_rb_read_bit(rb);
  }
  if (seq_params->frame_id_numbers_present_flag) {
    // We must always have delta_frame_id_length < frame_id_length,
    // in order for a frame to be referenced with a unique delta.
    // Avoid wasting bits by using a coding that enforces this restriction.
    seq_params->delta_frame_id_length = aom_rb_read_literal(rb, 4) + 2;
    seq_params->frame_id_length =
        aom_rb_read_literal(rb, 3) + seq_params->delta_frame_id_length + 1;
    if (seq_params->frame_id_length > 16)
      aom_internal_error(&cm->error, AOM_CODEC_CORRUPT_FRAME,
                         "Invalid frame_id_length");
  }

  setup_sb_size(seq_params, rb);

  seq_params->enable_filter_intra = aom_rb_read_bit(rb);
  seq_params->enable_intra_edge_filter = aom_rb_read_bit(rb);

  if (seq_params->reduced_still_picture_hdr) {
    seq_params->enable_interintra_compound = 0;
    seq_params->enable_masked_compound = 0;
    seq_params->enable_warped_motion = 0;
    seq_params->enable_dual_filter = 0;
    seq_params->order_hint_info.enable_order_hint = 0;
    seq_params->order_hint_info.enable_dist_wtd_comp = 0;
    seq_params->order_hint_info.enable_ref_frame_mvs = 0;
    seq_params->force_screen_content_tools = 2;  // SELECT_SCREEN_CONTENT_TOOLS
    seq_params->force_integer_mv = 2;            // SELECT_INTEGER_MV
    seq_params->order_hint_info.order_hint_bits_minus_1 = -1;
  } else {
    seq_params->enable_interintra_compound = aom_rb_read_bit(rb);
    seq_params->enable_masked_compound = aom_rb_read_bit(rb);
    seq_params->enable_warped_motion = aom_rb_read_bit(rb);
    seq_params->enable_dual_filter = aom_rb_read_bit(rb);

    seq_params->order_hint_info.enable_order_hint = aom_rb_read_bit(rb);
    seq_params->order_hint_info.enable_dist_wtd_comp =
        seq_params->order_hint_info.enable_order_hint ? aom_rb_read_bit(rb) : 0;
    seq_params->order_hint_info.enable_ref_frame_mvs =
        seq_params->order_hint_info.enable_order_hint ? aom_rb_read_bit(rb) : 0;

    if (aom_rb_read_bit(rb)) {
      seq_params->force_screen_content_tools =
          2;  // SELECT_SCREEN_CONTENT_TOOLS
    } else {
      seq_params->force_screen_content_tools = aom_rb_read_bit(rb);
    }

    if (seq_params->force_screen_content_tools > 0) {
      if (aom_rb_read_bit(rb)) {
        seq_params->force_integer_mv = 2;  // SELECT_INTEGER_MV
      } else {
        seq_params->force_integer_mv = aom_rb_read_bit(rb);
      }
    } else {
      seq_params->force_integer_mv = 2;  // SELECT_INTEGER_MV
    }
    seq_params->order_hint_info.order_hint_bits_minus_1 =
        seq_params->order_hint_info.enable_order_hint
            ? aom_rb_read_literal(rb, 3)
            : -1;
  }

  seq_params->enable_superres = aom_rb_read_bit(rb);
  seq_params->enable_cdef = aom_rb_read_bit(rb);
  seq_params->enable_restoration = aom_rb_read_bit(rb);
}

static int read_global_motion_params(WarpedMotionParams *params,
                                     const WarpedMotionParams *ref_params,
                                     struct aom_read_bit_buffer *rb,
                                     int allow_hp) {
  TransformationType type = aom_rb_read_bit(rb);
  if (type != IDENTITY) {
    if (aom_rb_read_bit(rb))
      type = ROTZOOM;
    else
      type = aom_rb_read_bit(rb) ? TRANSLATION : AFFINE;
  }

  *params = default_warp_params;
  params->wmtype = type;

  if (type >= ROTZOOM) {
    params->wmmat[2] = aom_rb_read_signed_primitive_refsubexpfin(
                           rb, GM_ALPHA_MAX + 1, SUBEXPFIN_K,
                           (ref_params->wmmat[2] >> GM_ALPHA_PREC_DIFF) -
                               (1 << GM_ALPHA_PREC_BITS)) *
                           GM_ALPHA_DECODE_FACTOR +
                       (1 << WARPEDMODEL_PREC_BITS);
    params->wmmat[3] = aom_rb_read_signed_primitive_refsubexpfin(
                           rb, GM_ALPHA_MAX + 1, SUBEXPFIN_K,
                           (ref_params->wmmat[3] >> GM_ALPHA_PREC_DIFF)) *
                       GM_ALPHA_DECODE_FACTOR;
  }

  if (type >= AFFINE) {
    params->wmmat[4] = aom_rb_read_signed_primitive_refsubexpfin(
                           rb, GM_ALPHA_MAX + 1, SUBEXPFIN_K,
                           (ref_params->wmmat[4] >> GM_ALPHA_PREC_DIFF)) *
                       GM_ALPHA_DECODE_FACTOR;
    params->wmmat[5] = aom_rb_read_signed_primitive_refsubexpfin(
                           rb, GM_ALPHA_MAX + 1, SUBEXPFIN_K,
                           (ref_params->wmmat[5] >> GM_ALPHA_PREC_DIFF) -
                               (1 << GM_ALPHA_PREC_BITS)) *
                           GM_ALPHA_DECODE_FACTOR +
                       (1 << WARPEDMODEL_PREC_BITS);
  } else {
    params->wmmat[4] = -params->wmmat[3];
    params->wmmat[5] = params->wmmat[2];
  }

  if (type >= TRANSLATION) {
    const int trans_bits = (type == TRANSLATION)
                               ? GM_ABS_TRANS_ONLY_BITS - !allow_hp
                               : GM_ABS_TRANS_BITS;
    const int trans_dec_factor =
        (type == TRANSLATION) ? GM_TRANS_ONLY_DECODE_FACTOR * (1 << !allow_hp)
                              : GM_TRANS_DECODE_FACTOR;
    const int trans_prec_diff = (type == TRANSLATION)
                                    ? GM_TRANS_ONLY_PREC_DIFF + !allow_hp
                                    : GM_TRANS_PREC_DIFF;
    params->wmmat[0] = aom_rb_read_signed_primitive_refsubexpfin(
                           rb, (1 << trans_bits) + 1, SUBEXPFIN_K,
                           (ref_params->wmmat[0] >> trans_prec_diff)) *
                       trans_dec_factor;
    params->wmmat[1] = aom_rb_read_signed_primitive_refsubexpfin(
                           rb, (1 << trans_bits) + 1, SUBEXPFIN_K,
                           (ref_params->wmmat[1] >> trans_prec_diff)) *
                       trans_dec_factor;
  }

  if (params->wmtype <= AFFINE) {
    int good_shear_params = get_shear_params(params);
    if (!good_shear_params) return 0;
  }

  return 1;
}

static void read_global_motion(AV1_COMMON *cm, struct aom_read_bit_buffer *rb) {
  for (int frame = LAST_FRAME; frame <= ALTREF_FRAME; ++frame) {
    const WarpedMotionParams *ref_params =
        cm->prev_frame ? &cm->prev_frame->global_motion[frame]
                       : &default_warp_params;
    int good_params = read_global_motion_params(
        &cm->global_motion[frame], ref_params, rb, cm->allow_high_precision_mv);
    if (!good_params) {
#if WARPED_MOTION_DEBUG
      printf("Warning: unexpected global motion shear params from aomenc\n");
#endif
      cm->global_motion[frame].invalid = 1;
    }

    // TODO(sarahparker, debargha): The logic in the commented out code below
    // does not work currently and causes mismatches when resize is on. Fix it
    // before turning the optimization back on.
    /*
    YV12_BUFFER_CONFIG *ref_buf = get_ref_frame(cm, frame);
    if (cm->width == ref_buf->y_crop_width &&
        cm->height == ref_buf->y_crop_height) {
      read_global_motion_params(&cm->global_motion[frame],
                                &cm->prev_frame->global_motion[frame], rb,
                                cm->allow_high_precision_mv);
    } else {
      cm->global_motion[frame] = default_warp_params;
    }
    */
    /*
    printf("Dec Ref %d [%d/%d]: %d %d %d %d\n",
           frame, cm->current_frame.frame_number, cm->show_frame,
           cm->global_motion[frame].wmmat[0],
           cm->global_motion[frame].wmmat[1],
           cm->global_motion[frame].wmmat[2],
           cm->global_motion[frame].wmmat[3]);
           */
  }
  memcpy(cm->cur_frame->global_motion, cm->global_motion,
         REF_FRAMES * sizeof(WarpedMotionParams));
}

// Release the references to the frame buffers in cm->ref_frame_map and reset
// all elements of cm->ref_frame_map to NULL.
static void reset_ref_frame_map(AV1_COMMON *const cm) {
  BufferPool *const pool = cm->buffer_pool;

  for (int i = 0; i < REF_FRAMES; i++) {
    decrease_ref_count(cm->ref_frame_map[i], pool);
    cm->ref_frame_map[i] = NULL;
  }
}

// Generate next_ref_frame_map.
static void generate_next_ref_frame_map(AV1Decoder *const pbi) {
  AV1_COMMON *const cm = &pbi->common;
  BufferPool *const pool = cm->buffer_pool;

  lock_buffer_pool(pool);
  // cm->next_ref_frame_map holds references to frame buffers. After storing a
  // frame buffer index in cm->next_ref_frame_map, we need to increase the
  // frame buffer's ref_count.
  int ref_index = 0;
  for (int mask = cm->current_frame.refresh_frame_flags; mask; mask >>= 1) {
    if (mask & 1) {
      cm->next_ref_frame_map[ref_index] = cm->cur_frame;
    } else {
      cm->next_ref_frame_map[ref_index] = cm->ref_frame_map[ref_index];
    }
    if (cm->next_ref_frame_map[ref_index] != NULL)
      ++cm->next_ref_frame_map[ref_index]->ref_count;
    ++ref_index;
  }

  for (; ref_index < REF_FRAMES; ++ref_index) {
    cm->next_ref_frame_map[ref_index] = cm->ref_frame_map[ref_index];
    if (cm->next_ref_frame_map[ref_index] != NULL)
      ++cm->next_ref_frame_map[ref_index]->ref_count;
  }
  unlock_buffer_pool(pool);
  pbi->hold_ref_buf = 1;
}

// If the refresh_frame_flags bitmask is set, update reference frame id values
// and mark frames as valid for reference.
static void update_ref_frame_id(AV1_COMMON *const cm, int frame_id) {
  assert(cm->seq_params.frame_id_numbers_present_flag);
  int refresh_frame_flags = cm->current_frame.refresh_frame_flags;
  for (int i = 0; i < REF_FRAMES; i++) {
    if ((refresh_frame_flags >> i) & 1) {
      cm->ref_frame_id[i] = frame_id;
      cm->valid_for_referencing[i] = 1;
    }
  }
}

static void show_existing_frame_reset(AV1Decoder *const pbi,
                                      int existing_frame_idx) {
  AV1_COMMON *const cm = &pbi->common;

  assert(cm->show_existing_frame);

  cm->current_frame.frame_type = KEY_FRAME;

  cm->current_frame.refresh_frame_flags = (1 << REF_FRAMES) - 1;

  for (int i = 0; i < INTER_REFS_PER_FRAME; ++i) {
    cm->remapped_ref_idx[i] = INVALID_IDX;
  }

  if (pbi->need_resync) {
    reset_ref_frame_map(cm);
    pbi->need_resync = 0;
  }

  // Note that the displayed frame must be valid for referencing in order to
  // have been selected.
  if (cm->seq_params.frame_id_numbers_present_flag) {
    update_ref_frame_id(cm, cm->ref_frame_id[existing_frame_idx]);
  }

  cm->refresh_frame_context = REFRESH_FRAME_CONTEXT_DISABLED;

  generate_next_ref_frame_map(pbi);

  // Reload the adapted CDFs from when we originally coded this keyframe
  *cm->fc = cm->next_ref_frame_map[existing_frame_idx]->frame_context;
}

static INLINE void reset_frame_buffers(AV1_COMMON *cm) {
  RefCntBuffer *const frame_bufs = cm->buffer_pool->frame_bufs;
  int i;

  // We have not stored any references to frame buffers in
  // cm->next_ref_frame_map, so we can directly reset it to all NULL.
  for (i = 0; i < REF_FRAMES; ++i) {
    cm->next_ref_frame_map[i] = NULL;
  }

  lock_buffer_pool(cm->buffer_pool);
  reset_ref_frame_map(cm);
  assert(cm->cur_frame->ref_count == 1);
  for (i = 0; i < FRAME_BUFFERS; ++i) {
    // Reset all unreferenced frame buffers. We can also reset cm->cur_frame
    // because we are the sole owner of cm->cur_frame.
    if (frame_bufs[i].ref_count > 0 && &frame_bufs[i] != cm->cur_frame) {
      continue;
    }
    frame_bufs[i].order_hint = 0;
    av1_zero(frame_bufs[i].ref_order_hints);
  }
  av1_zero_unused_internal_frame_buffers(&cm->buffer_pool->int_frame_buffers);
  unlock_buffer_pool(cm->buffer_pool);
}

// On success, returns 0. On failure, calls aom_internal_error and does not
// return.
static int read_uncompressed_header(AV1Decoder *pbi,
                                    struct aom_read_bit_buffer *rb) {
  AV1_COMMON *const cm = &pbi->common;
  const SequenceHeader *const seq_params = &cm->seq_params;
  CurrentFrame *const current_frame = &cm->current_frame;
  MACROBLOCKD *const xd = &pbi->mb;
  BufferPool *const pool = cm->buffer_pool;
  RefCntBuffer *const frame_bufs = pool->frame_bufs;

  if (!pbi->sequence_header_ready) {
    aom_internal_error(&cm->error, AOM_CODEC_CORRUPT_FRAME,
                       "No sequence header");
  }

  cm->last_frame_type = current_frame->frame_type;

  if (seq_params->reduced_still_picture_hdr) {
    cm->show_existing_frame = 0;
    cm->show_frame = 1;
    current_frame->frame_type = KEY_FRAME;
    if (pbi->sequence_header_changed) {
      // This is the start of a new coded video sequence.
      pbi->sequence_header_changed = 0;
      pbi->decoding_first_frame = 1;
      reset_frame_buffers(cm);
    }
    cm->error_resilient_mode = 1;
  } else {
    cm->show_existing_frame = aom_rb_read_bit(rb);
    pbi->reset_decoder_state = 0;

    if (cm->show_existing_frame) {
      if (pbi->sequence_header_changed) {
        aom_internal_error(
            &cm->error, AOM_CODEC_CORRUPT_FRAME,
            "New sequence header starts with a show_existing_frame.");
      }
      // Show an existing frame directly.
      const int existing_frame_idx = aom_rb_read_literal(rb, 3);
      RefCntBuffer *const frame_to_show = cm->ref_frame_map[existing_frame_idx];
      if (frame_to_show == NULL) {
        aom_internal_error(&cm->error, AOM_CODEC_UNSUP_BITSTREAM,
                           "Buffer does not contain a decoded frame");
      }
      if (seq_params->decoder_model_info_present_flag &&
          cm->timing_info.equal_picture_interval == 0) {
        av1_read_temporal_point_info(cm, rb);
      }
      if (seq_params->frame_id_numbers_present_flag) {
        int frame_id_length = seq_params->frame_id_length;
        int display_frame_id = aom_rb_read_literal(rb, frame_id_length);
        /* Compare display_frame_id with ref_frame_id and check valid for
         * referencing */
        if (display_frame_id != cm->ref_frame_id[existing_frame_idx] ||
            cm->valid_for_referencing[existing_frame_idx] == 0)
          aom_internal_error(&cm->error, AOM_CODEC_CORRUPT_FRAME,
                             "Reference buffer frame ID mismatch");
      }
      lock_buffer_pool(pool);
      assert(frame_to_show->ref_count > 0);
      // cm->cur_frame should be the buffer referenced by the return value
      // of the get_free_fb() call in av1_receive_compressed_data(), and
      // generate_next_ref_frame_map() has not been called, so ref_count
      // should still be 1.
      assert(cm->cur_frame->ref_count == 1);
      // assign_frame_buffer_p() decrements ref_count directly rather than
      // call decrease_ref_count(). If cm->cur_frame->raw_frame_buffer has
      // already been allocated, it will not be released by
      // assign_frame_buffer_p()!
      assert(!cm->cur_frame->raw_frame_buffer.data);
      assign_frame_buffer_p(&cm->cur_frame, frame_to_show);
      pbi->reset_decoder_state = frame_to_show->frame_type == KEY_FRAME;
      unlock_buffer_pool(pool);

      cm->lf.filter_level[0] = 0;
      cm->lf.filter_level[1] = 0;
      cm->show_frame = 1;

      if (!frame_to_show->showable_frame) {
        aom_merge_corrupted_flag(&xd->corrupted, 1);
      }
      if (pbi->reset_decoder_state) frame_to_show->showable_frame = 0;

      cm->film_grain_params = frame_to_show->film_grain_params;

      if (pbi->reset_decoder_state) {
        show_existing_frame_reset(pbi, existing_frame_idx);
      } else {
        current_frame->refresh_frame_flags = 0;
      }
      av1_decode_sef(pbi);
      return 0;
    }

    current_frame->frame_type = (FRAME_TYPE)aom_rb_read_literal(rb, 2);
    if (pbi->sequence_header_changed) {
      if (current_frame->frame_type == KEY_FRAME) {
        // This is the start of a new coded video sequence.
        pbi->sequence_header_changed = 0;
        pbi->decoding_first_frame = 1;
        reset_frame_buffers(cm);
      } else {
        aom_internal_error(&cm->error, AOM_CODEC_CORRUPT_FRAME,
                           "Sequence header has changed without a keyframe.");
      }
    }

    cm->show_frame = aom_rb_read_bit(rb);
    if (seq_params->still_picture &&
        (current_frame->frame_type != KEY_FRAME || !cm->show_frame)) {
      aom_internal_error(&cm->error, AOM_CODEC_CORRUPT_FRAME,
                         "Still pictures must be coded as shown keyframes");
    }
    cm->showable_frame = current_frame->frame_type != KEY_FRAME;
    if (cm->show_frame) {
      if (seq_params->decoder_model_info_present_flag &&
          cm->timing_info.equal_picture_interval == 0)
        av1_read_temporal_point_info(cm, rb);
    } else {
      // See if this frame can be used as show_existing_frame in future
      cm->showable_frame = aom_rb_read_bit(rb);
    }
    cm->cur_frame->showable_frame = cm->showable_frame;
    cm->error_resilient_mode =
        frame_is_sframe(cm) ||
                (current_frame->frame_type == KEY_FRAME && cm->show_frame)
            ? 1
            : aom_rb_read_bit(rb);
  }

  cm->disable_cdf_update = aom_rb_read_bit(rb);
  if (seq_params->force_screen_content_tools == 2) {
    cm->allow_screen_content_tools = aom_rb_read_bit(rb);
  } else {
    cm->allow_screen_content_tools = seq_params->force_screen_content_tools;
  }

  if (cm->allow_screen_content_tools) {
    if (seq_params->force_integer_mv == 2) {
      cm->cur_frame_force_integer_mv = aom_rb_read_bit(rb);
    } else {
      cm->cur_frame_force_integer_mv = seq_params->force_integer_mv;
    }
  } else {
    cm->cur_frame_force_integer_mv = 0;
  }

  int frame_size_override_flag = 0;
  cm->allow_intrabc = 0;
  cm->primary_ref_frame = PRIMARY_REF_NONE;

  if (!seq_params->reduced_still_picture_hdr) {
    if (seq_params->frame_id_numbers_present_flag) {
      int frame_id_length = seq_params->frame_id_length;
      int diff_len = seq_params->delta_frame_id_length;
      int prev_frame_id = 0;
      int have_prev_frame_id =
          !pbi->decoding_first_frame &&
          !(current_frame->frame_type == KEY_FRAME && cm->show_frame);
      if (have_prev_frame_id) {
        prev_frame_id = cm->current_frame_id;
      }
      cm->current_frame_id = aom_rb_read_literal(rb, frame_id_length);

      if (have_prev_frame_id) {
        int diff_frame_id;
        if (cm->current_frame_id > prev_frame_id) {
          diff_frame_id = cm->current_frame_id - prev_frame_id;
        } else {
          diff_frame_id =
              (1 << frame_id_length) + cm->current_frame_id - prev_frame_id;
        }
        /* Check current_frame_id for conformance */
        if (prev_frame_id == cm->current_frame_id ||
            diff_frame_id >= (1 << (frame_id_length - 1))) {
          aom_internal_error(&cm->error, AOM_CODEC_CORRUPT_FRAME,
                             "Invalid value of current_frame_id");
        }
      }
      /* Check if some frames need to be marked as not valid for referencing */
      for (int i = 0; i < REF_FRAMES; i++) {
        if (current_frame->frame_type == KEY_FRAME && cm->show_frame) {
          cm->valid_for_referencing[i] = 0;
        } else if (cm->current_frame_id - (1 << diff_len) > 0) {
          if (cm->ref_frame_id[i] > cm->current_frame_id ||
              cm->ref_frame_id[i] < cm->current_frame_id - (1 << diff_len))
            cm->valid_for_referencing[i] = 0;
        } else {
          if (cm->ref_frame_id[i] > cm->current_frame_id &&
              cm->ref_frame_id[i] < (1 << frame_id_length) +
                                        cm->current_frame_id - (1 << diff_len))
            cm->valid_for_referencing[i] = 0;
        }
      }
    }

    frame_size_override_flag = frame_is_sframe(cm) ? 1 : aom_rb_read_bit(rb);

    current_frame->order_hint = aom_rb_read_literal(
        rb, seq_params->order_hint_info.order_hint_bits_minus_1 + 1);
    current_frame->frame_number = current_frame->order_hint;

    if (!cm->error_resilient_mode && !frame_is_intra_only(cm)) {
      cm->primary_ref_frame = aom_rb_read_literal(rb, PRIMARY_REF_BITS);
    }
  }

  if (seq_params->decoder_model_info_present_flag) {
    cm->buffer_removal_time_present = aom_rb_read_bit(rb);
    if (cm->buffer_removal_time_present) {
      for (int op_num = 0;
           op_num < seq_params->operating_points_cnt_minus_1 + 1; op_num++) {
        if (cm->op_params[op_num].decoder_model_param_present_flag) {
          if ((((seq_params->operating_point_idc[op_num] >>
                 cm->temporal_layer_id) &
                0x1) &&
               ((seq_params->operating_point_idc[op_num] >>
                 (cm->spatial_layer_id + 8)) &
                0x1)) ||
              seq_params->operating_point_idc[op_num] == 0) {
            cm->op_frame_timing[op_num].buffer_removal_time =
                aom_rb_read_unsigned_literal(
                    rb, cm->buffer_model.buffer_removal_time_length);
          } else {
            cm->op_frame_timing[op_num].buffer_removal_time = 0;
          }
        } else {
          cm->op_frame_timing[op_num].buffer_removal_time = 0;
        }
      }
    }
  }
  if (current_frame->frame_type == KEY_FRAME) {
    if (!cm->show_frame) {  // unshown keyframe (forward keyframe)
      current_frame->refresh_frame_flags = aom_rb_read_literal(rb, REF_FRAMES);
    } else {  // shown keyframe
      current_frame->refresh_frame_flags = (1 << REF_FRAMES) - 1;
    }

    for (int i = 0; i < INTER_REFS_PER_FRAME; ++i) {
      cm->remapped_ref_idx[i] = INVALID_IDX;
    }
    if (pbi->need_resync) {
      reset_ref_frame_map(cm);
      pbi->need_resync = 0;
    }
  } else {
    if (current_frame->frame_type == INTRA_ONLY_FRAME) {
      current_frame->refresh_frame_flags = aom_rb_read_literal(rb, REF_FRAMES);
      if (current_frame->refresh_frame_flags == 0xFF) {
        aom_internal_error(&cm->error, AOM_CODEC_UNSUP_BITSTREAM,
                           "Intra only frames cannot have refresh flags 0xFF");
      }
      if (pbi->need_resync) {
        reset_ref_frame_map(cm);
        pbi->need_resync = 0;
      }
    } else if (pbi->need_resync != 1) { /* Skip if need resync */
      current_frame->refresh_frame_flags =
          frame_is_sframe(cm) ? 0xFF : aom_rb_read_literal(rb, REF_FRAMES);
    }
  }

  if (!frame_is_intra_only(cm) || current_frame->refresh_frame_flags != 0xFF) {
    // Read all ref frame order hints if error_resilient_mode == 1
    if (cm->error_resilient_mode &&
        seq_params->order_hint_info.enable_order_hint) {
      for (int ref_idx = 0; ref_idx < REF_FRAMES; ref_idx++) {
        // Read order hint from bit stream
        unsigned int order_hint = aom_rb_read_literal(
            rb, seq_params->order_hint_info.order_hint_bits_minus_1 + 1);
        // Get buffer
        RefCntBuffer *buf = cm->ref_frame_map[ref_idx];
        if (buf == NULL || order_hint != buf->order_hint) {
          if (buf != NULL) {
            lock_buffer_pool(pool);
            decrease_ref_count(buf, pool);
            unlock_buffer_pool(pool);
          }
          // If no corresponding buffer exists, allocate a new buffer with all
          // pixels set to neutral grey.
          int buf_idx = get_free_fb(cm);
          if (buf_idx == INVALID_IDX) {
            aom_internal_error(&cm->error, AOM_CODEC_MEM_ERROR,
                               "Unable to find free frame buffer");
          }
          buf = &frame_bufs[buf_idx];
          lock_buffer_pool(pool);

          if (av1_reallocate_frame_buffer(pool->cb_priv, &buf->buf,
                                          seq_params->max_frame_width,
                                          seq_params->max_frame_height,
                                          seq_params->max_frame_width,
                                          seq_params->use_highbitdepth))
          {
              unlock_buffer_pool(pool);
              aom_internal_error(&cm->error, AOM_CODEC_MEM_ERROR, "Failed to allocate frame buffer");
          }
          unlock_buffer_pool(pool);
          set_planes_to_neutral_grey(seq_params, &buf->buf, 0);
          cm->ref_frame_map[ref_idx] = buf;
          buf->order_hint = order_hint;
        }
      }
    }
  }

  if (current_frame->frame_type == KEY_FRAME) {
    setup_frame_size(cm, frame_size_override_flag, rb);

    if (cm->allow_screen_content_tools && !av1_superres_scaled(cm))
      cm->allow_intrabc = aom_rb_read_bit(rb);
    cm->allow_ref_frame_mvs = 0;
    cm->prev_frame = NULL;
  } else {
    cm->allow_ref_frame_mvs = 0;

    if (current_frame->frame_type == INTRA_ONLY_FRAME) {
      cm->cur_frame->film_grain_params_present =
          seq_params->film_grain_params_present;
      setup_frame_size(cm, frame_size_override_flag, rb);
      if (cm->allow_screen_content_tools && !av1_superres_scaled(cm))
        cm->allow_intrabc = aom_rb_read_bit(rb);

    } else if (pbi->need_resync != 1) { /* Skip if need resync */
      int frame_refs_short_signaling = 0;
      // Frame refs short signaling is off when error resilient mode is on.
      if (seq_params->order_hint_info.enable_order_hint)
        frame_refs_short_signaling = aom_rb_read_bit(rb);

      if (frame_refs_short_signaling) {
        // == LAST_FRAME ==
        const int lst_ref = aom_rb_read_literal(rb, REF_FRAMES_LOG2);
        const RefCntBuffer *const lst_buf = cm->ref_frame_map[lst_ref];

        // == GOLDEN_FRAME ==
        const int gld_ref = aom_rb_read_literal(rb, REF_FRAMES_LOG2);
        const RefCntBuffer *const gld_buf = cm->ref_frame_map[gld_ref];

        // Most of the time, streams start with a keyframe. In that case,
        // ref_frame_map will have been filled in at that point and will not
        // contain any NULLs. However, streams are explicitly allowed to start
        // with an intra-only frame, so long as they don't then signal a
        // reference to a slot that hasn't been set yet. That's what we are
        // checking here.
        if (lst_buf == NULL)
          aom_internal_error(&cm->error, AOM_CODEC_CORRUPT_FRAME,
                             "Inter frame requests nonexistent reference");
        if (gld_buf == NULL)
          aom_internal_error(&cm->error, AOM_CODEC_CORRUPT_FRAME,
                             "Inter frame requests nonexistent reference");

        av1_set_frame_refs(cm, cm->remapped_ref_idx, lst_ref, gld_ref);
      }

      for (int i = 0; i < INTER_REFS_PER_FRAME; ++i) {
        int ref = 0;
        if (!frame_refs_short_signaling) {
          ref = aom_rb_read_literal(rb, REF_FRAMES_LOG2);

          // Most of the time, streams start with a keyframe. In that case,
          // ref_frame_map will have been filled in at that point and will not
          // contain any -1's. However, streams are explicitly allowed to start
          // with an intra-only frame, so long as they don't then signal a
          // reference to a slot that hasn't been set yet. That's what we are
          // checking here.
          if (cm->ref_frame_map[ref] == NULL)
            aom_internal_error(&cm->error, AOM_CODEC_CORRUPT_FRAME,
                               "Inter frame requests nonexistent reference");
          cm->remapped_ref_idx[i] = ref;
        } else {
          ref = cm->remapped_ref_idx[i];
        }

        cm->ref_frame_sign_bias[LAST_FRAME + i] = 0;

        if (seq_params->frame_id_numbers_present_flag) {
          int frame_id_length = seq_params->frame_id_length;
          int diff_len = seq_params->delta_frame_id_length;
          int delta_frame_id_minus_1 = aom_rb_read_literal(rb, diff_len);
          int ref_frame_id =
              ((cm->current_frame_id - (delta_frame_id_minus_1 + 1) +
                (1 << frame_id_length)) %
               (1 << frame_id_length));
          // Compare values derived from delta_frame_id_minus_1 and
          // refresh_frame_flags. Also, check valid for referencing
          if (ref_frame_id != cm->ref_frame_id[ref] ||
              cm->valid_for_referencing[ref] == 0)
            aom_internal_error(&cm->error, AOM_CODEC_CORRUPT_FRAME,
                               "Reference buffer frame ID mismatch");
        }
      }

      if (!cm->error_resilient_mode && frame_size_override_flag) {
        setup_frame_size_with_refs(cm, rb);
      } else {
        setup_frame_size(cm, frame_size_override_flag, rb);
      }

      if (cm->cur_frame_force_integer_mv) {
        cm->allow_high_precision_mv = 0;
      } else {
        cm->allow_high_precision_mv = aom_rb_read_bit(rb);
      }
      cm->interp_filter = read_frame_interp_filter(rb);
      cm->switchable_motion_mode = aom_rb_read_bit(rb);
    }

    cm->prev_frame = get_primary_ref_frame_buf(cm);
    if (cm->primary_ref_frame != PRIMARY_REF_NONE &&
        get_primary_ref_frame_buf(cm) == NULL) {
      aom_internal_error(&cm->error, AOM_CODEC_CORRUPT_FRAME,
                         "Reference frame containing this frame's initial "
                         "frame context is unavailable.");
    }

    if (!(current_frame->frame_type == INTRA_ONLY_FRAME) &&
        pbi->need_resync != 1) {
      if (frame_might_allow_ref_frame_mvs(cm))
        cm->allow_ref_frame_mvs = aom_rb_read_bit(rb);
      else
        cm->allow_ref_frame_mvs = 0;

      for (int i = LAST_FRAME; i <= ALTREF_FRAME; ++i) {
        const RefCntBuffer *const ref_buf = get_ref_frame_buf(cm, i);
        struct scale_factors *const ref_scale_factors =
            get_ref_scale_factors(cm, i);
        av1_setup_scale_factors_for_frame(
            ref_scale_factors, ref_buf->buf.y_crop_width,
            ref_buf->buf.y_crop_height, cm->width, cm->height);
        if ((!av1_is_valid_scale(ref_scale_factors)))
          aom_internal_error(&cm->error, AOM_CODEC_UNSUP_BITSTREAM,
                             "Reference frame has invalid dimensions");
      }
    }
  }

  av1_setup_frame_buf_refs(cm);

  av1_setup_frame_sign_bias(cm);

  cm->cur_frame->frame_type = current_frame->frame_type;

  if (seq_params->frame_id_numbers_present_flag) {
    update_ref_frame_id(cm, cm->current_frame_id);
  }

  const int might_bwd_adapt =
      !(seq_params->reduced_still_picture_hdr) && !(cm->disable_cdf_update);
  if (might_bwd_adapt) {
    cm->refresh_frame_context = aom_rb_read_bit(rb)
                                    ? REFRESH_FRAME_CONTEXT_DISABLED
                                    : REFRESH_FRAME_CONTEXT_BACKWARD;
  } else {
    cm->refresh_frame_context = REFRESH_FRAME_CONTEXT_DISABLED;
  }

  cm->cur_frame->buf.bit_depth = seq_params->bit_depth;
  cm->cur_frame->buf.color_primaries = seq_params->color_primaries;
  cm->cur_frame->buf.transfer_characteristics =
      seq_params->transfer_characteristics;
  cm->cur_frame->buf.matrix_coefficients = seq_params->matrix_coefficients;
  cm->cur_frame->buf.monochrome = seq_params->monochrome;
  cm->cur_frame->buf.chroma_sample_position =
      seq_params->chroma_sample_position;
  cm->cur_frame->buf.color_range = seq_params->color_range;
  cm->cur_frame->buf.render_width = cm->render_width;
  cm->cur_frame->buf.render_height = cm->render_height;

  if (pbi->need_resync) {
    aom_internal_error(&cm->error, AOM_CODEC_CORRUPT_FRAME,
                       "Keyframe / intra-only frame required to reset decoder"
                       " state");
  }

  generate_next_ref_frame_map(pbi);

  if (cm->allow_intrabc) {
    // Set parameters corresponding to no filtering.
    struct loopfilter *lf = &cm->lf;
    lf->filter_level[0] = 0;
    lf->filter_level[1] = 0;
    cm->cdef_info.cdef_bits = 0;
    cm->cdef_info.cdef_strengths[0] = 0;
    cm->cdef_info.nb_cdef_strengths = 1;
    cm->cdef_info.cdef_uv_strengths[0] = 0;
    cm->rst_info[0].frame_restoration_type = RESTORE_NONE;
    cm->rst_info[1].frame_restoration_type = RESTORE_NONE;
    cm->rst_info[2].frame_restoration_type = RESTORE_NONE;
  }

  read_tile_info(pbi, rb);
  setup_quantization(cm, rb);
  xd->bd = (int)seq_params->bit_depth;

  //if (cm->num_allocated_above_context_planes < av1_num_planes(cm) ||
  //    cm->num_allocated_above_context_mi_col < cm->mi_cols ||
  //    cm->num_allocated_above_contexts < cm->tile_rows) {
  //  av1_free_above_context_buffers(cm, cm->num_allocated_above_contexts);
  //  if (av1_alloc_above_context_buffers(cm, cm->tile_rows))
  //    aom_internal_error(&cm->error, AOM_CODEC_MEM_ERROR,
  //                       "Failed to allocate context buffers");
  //}
  av1_setup_context_buffers(pbi);

  if (cm->primary_ref_frame == PRIMARY_REF_NONE) {
    av1_setup_past_independence(cm);
  }

  setup_segmentation(cm, rb);

  cm->delta_q_info.delta_q_res = 1;
  cm->delta_q_info.delta_lf_res = 1;
  cm->delta_q_info.delta_lf_present_flag = 0;
  cm->delta_q_info.delta_lf_multi = 0;
  cm->delta_q_info.delta_q_present_flag =
      cm->base_qindex > 0 ? aom_rb_read_bit(rb) : 0;
  if (cm->delta_q_info.delta_q_present_flag) {
    xd->current_qindex = cm->base_qindex;
    cm->delta_q_info.delta_q_res = 1 << aom_rb_read_literal(rb, 2);
    if (!cm->allow_intrabc)
      cm->delta_q_info.delta_lf_present_flag = aom_rb_read_bit(rb);
    if (cm->delta_q_info.delta_lf_present_flag) {
      cm->delta_q_info.delta_lf_res = 1 << aom_rb_read_literal(rb, 2);
      cm->delta_q_info.delta_lf_multi = aom_rb_read_bit(rb);
      av1_reset_loop_filter_delta(xd, av1_num_planes(cm));
    }
  }

  xd->cur_frame_force_integer_mv = cm->cur_frame_force_integer_mv;

  for (int i = 0; i < MAX_SEGMENTS; ++i) {
    const int qindex = av1_get_qindex(&cm->seg, i, cm->base_qindex);
    xd->lossless[i] = qindex == 0 && cm->y_dc_delta_q == 0 &&
                      cm->u_dc_delta_q == 0 && cm->u_ac_delta_q == 0 &&
                      cm->v_dc_delta_q == 0 && cm->v_ac_delta_q == 0;
    xd->qindex[i] = qindex;
  }
  cm->coded_lossless = is_coded_lossless(cm, xd);
  cm->all_lossless = cm->coded_lossless && !av1_superres_scaled(cm);
  setup_segmentation_dequant(cm, xd);
  if (cm->coded_lossless) {
    cm->lf.filter_level[0] = 0;
    cm->lf.filter_level[1] = 0;
  }
  if (cm->coded_lossless || !seq_params->enable_cdef) {
    cm->cdef_info.cdef_bits = 0;
    cm->cdef_info.cdef_strengths[0] = 0;
    cm->cdef_info.cdef_uv_strengths[0] = 0;
  }
  if (cm->all_lossless || !seq_params->enable_restoration) {
    cm->rst_info[0].frame_restoration_type = RESTORE_NONE;
    cm->rst_info[1].frame_restoration_type = RESTORE_NONE;
    cm->rst_info[2].frame_restoration_type = RESTORE_NONE;
  }
  setup_loopfilter(cm, rb);

  if (!cm->coded_lossless && seq_params->enable_cdef) {
    setup_cdef(cm, rb);
  }
  if (!cm->all_lossless && seq_params->enable_restoration) {
    decode_restoration_mode(cm, rb);
  }

  cm->tx_mode = read_tx_mode(cm, rb);
  current_frame->reference_mode = read_frame_reference_mode(cm, rb);
  if (current_frame->reference_mode != SINGLE_REFERENCE)
    setup_compound_reference_mode(cm);

  av1_setup_skip_mode_allowed(cm);
  current_frame->skip_mode_info.skip_mode_flag =
      current_frame->skip_mode_info.skip_mode_allowed ? aom_rb_read_bit(rb) : 0;

  if (frame_might_allow_warped_motion(cm))
    cm->allow_warped_motion = aom_rb_read_bit(rb);
  else
    cm->allow_warped_motion = 0;

  cm->reduced_tx_set_used = aom_rb_read_bit(rb);

  if (cm->allow_ref_frame_mvs && !frame_might_allow_ref_frame_mvs(cm)) {
    aom_internal_error(&cm->error, AOM_CODEC_CORRUPT_FRAME,
                       "Frame wrongly requests reference frame MVs");
  }

  if (!frame_is_intra_only(cm)) read_global_motion(cm, rb);

  cm->cur_frame->film_grain_params_present =
      seq_params->film_grain_params_present;
  read_film_grain(cm, rb);

#if EXT_TILE_DEBUG
  if (pbi->ext_tile_debug && cm->large_scale_tile) {
    read_ext_tile_info(pbi, rb);
    av1_set_single_tile_decoding_mode(cm);
  }
#endif  // EXT_TILE_DEBUG
  return 0;
}

struct aom_read_bit_buffer *av1_init_read_bit_buffer(
    AV1Decoder *pbi, struct aom_read_bit_buffer *rb, const uint8_t *data,
    const uint8_t *data_end) {
  rb->bit_offset = 0;
  rb->error_handler = error_handler;
  rb->error_handler_data = &pbi->common;
  rb->bit_buffer = data;
  rb->bit_buffer_end = data_end;
  return rb;
}

void av1_read_frame_size(struct aom_read_bit_buffer *rb, int num_bits_width,
                         int num_bits_height, int *width, int *height) {
  *width = aom_rb_read_literal(rb, num_bits_width) + 1;
  *height = aom_rb_read_literal(rb, num_bits_height) + 1;
}

BITSTREAM_PROFILE av1_read_profile(struct aom_read_bit_buffer *rb) {
  int profile = aom_rb_read_literal(rb, PROFILE_BITS);
  return (BITSTREAM_PROFILE)profile;
}

uint32_t av1_decode_frame_headers_and_setup(AV1Decoder *pbi,
                                            struct aom_read_bit_buffer *rb,
                                            const uint8_t *data,
                                            const uint8_t **p_data_end,
                                            int trailing_bits_present) {
  AV1_COMMON *const cm = &pbi->common;
  const int num_planes = av1_num_planes(cm);
  MACROBLOCKD *const xd = &pbi->mb;

#if CONFIG_BITSTREAM_DEBUG
  bitstream_queue_set_frame_read(cm->current_frame.frame_number * 2 +
                                 cm->show_frame);
#endif
#if CONFIG_MISMATCH_DEBUG
  mismatch_move_frame_idx_r();
#endif

  for (int i = LAST_FRAME; i <= ALTREF_FRAME; ++i) {
    cm->global_motion[i] = default_warp_params;
    cm->cur_frame->global_motion[i] = default_warp_params;
  }
  xd->global_motion = cm->global_motion;

  read_uncompressed_header(pbi, rb);

  if (trailing_bits_present) av1_check_trailing_bits(pbi, rb);

  // If cm->single_tile_decoding = 0, the independent decoding of a single tile
  // or a section of a frame is not allowed.
  if (!cm->single_tile_decoding &&
      (pbi->dec_tile_row >= 0 || pbi->dec_tile_col >= 0)) {
    pbi->dec_tile_row = -1;
    pbi->dec_tile_col = -1;
  }

  const uint32_t uncomp_hdr_size =
      (uint32_t)aom_rb_bytes_read(rb);  // Size of the uncompressed header
  YV12_BUFFER_CONFIG *new_fb = &cm->cur_frame->buf;
  xd->cur_buf = new_fb;
  if (av1_allow_intrabc(cm)) {
    av1_setup_scale_factors_for_frame(
        &cm->sf_identity, xd->cur_buf->y_crop_width, xd->cur_buf->y_crop_height,
        xd->cur_buf->y_crop_width, xd->cur_buf->y_crop_height);
  }

  if (cm->show_existing_frame) {
    // showing a frame directly
    *p_data_end = data + uncomp_hdr_size;
    if (pbi->reset_decoder_state) {
      // Use the default frame context values.
      *cm->fc = *cm->default_frame_context;
      if (!cm->fc->initialized)
        aom_internal_error(&cm->error, AOM_CODEC_CORRUPT_FRAME,
                           "Uninitialized entropy context.");
    }
    return uncomp_hdr_size;
  }
  decode_mt_init(pbi);
  av1_setup_motion_field(pbi);

  av1_setup_block_planes(xd, cm->seq_params.subsampling_x,
                         cm->seq_params.subsampling_y, num_planes);
  if (cm->primary_ref_frame == PRIMARY_REF_NONE) {
    // use the default frame context values
    *cm->fc = *cm->default_frame_context;
  } else {
    *cm->fc = get_primary_ref_frame_buf(cm)->frame_context;
  }
  if (!cm->fc->initialized)
    aom_internal_error(&cm->error, AOM_CODEC_CORRUPT_FRAME,
                       "Uninitialized entropy context.");

  xd->corrupted = 0;
  return uncomp_hdr_size;
}

// Once-per-frame initialization
static void setup_frame_info(AV1Decoder *pbi) {
  AV1_COMMON *const cm = &pbi->common;

  if (cm->rst_info[0].frame_restoration_type != RESTORE_NONE ||
      cm->rst_info[1].frame_restoration_type != RESTORE_NONE ||
      cm->rst_info[2].frame_restoration_type != RESTORE_NONE) {
    av1_alloc_restoration_buffers(cm);
  }
}

void av1_decode_tg_tiles_and_wrapup(AV1Decoder *pbi, const uint8_t *data,
                                    const uint8_t *data_end,
                                    const uint8_t **p_data_end, int start_tile,
                                    int end_tile, int initialize_flag) {
  AV1_COMMON *const cm = &pbi->common;
  MACROBLOCKD *const xd = &pbi->mb;
  if (initialize_flag) {
    setup_frame_info(pbi);
    av1_setup_frame(pbi, cm);
  }
  {
    const int64_t perf_t0 = av1_perf_ticks();
    *p_data_end = decode_tiles_mt(pbi, data, data_end, start_tile, end_tile);
    av1_perf_cpu_add(pbi, AV1_PERF_CPU_ENTROPY_WALL, perf_t0);
  }

  if (end_tile != cm->tile_rows * cm->tile_cols - 1) {
    return;
  }
  
  if (!xd->corrupted) {
    if (cm->refresh_frame_context == REFRESH_FRAME_CONTEXT_BACKWARD) {
      assert(cm->context_update_tile_id < pbi->allocated_tiles);
      *cm->fc = pbi->ctx_the_choosen_one;
      av1_reset_cdf_symbol_counters(cm->fc);
    }
  } else {
    aom_internal_error(&cm->error, AOM_CODEC_CORRUPT_FRAME,
                       "Decode failed. Frame data is corrupted.");
  }

  av1_decode_frame_gpu(pbi);

  // Non frame parallel update frame context here.
  if (!cm->large_scale_tile) {
    cm->cur_frame->frame_context = *cm->fc;
  }
}
