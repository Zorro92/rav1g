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

#define Warp 0
#define CasualInter 1
#define CompoundAvrg 2
#define CompoundDiff 3
#define CompoundMasked 4
#define CompoundGlobalWarp 5
#define CompoundDiffUv 6
#define CompoundDiffUvGlobalWarp 7
#define ObmcAbove 8
#define ObmcLeft 9
#define Inter2x2 10
#define Inter2x2Comp 11
#define Inter2x2CompP2 12
#define InterSizesAllCommon 24
#define Inter2x2ArrOffset 216
#define Inter2x2Count 3
#define InterCountsAll 219

#define CompoundTypeAvrg 0
#define CompoundTypeMasked 1
#define CompoundTypeDiffY 2
#define CompoundTypeDiffUv 3

#define IntraSizes 9
#define ReconstructBlockSizes 36
#define IntraTypeCount 10
#define IntraBlockOffset 264
#define ReconBlockOffset 219

#define DC_PRED 0
#define V_PRED 1
#define H_PRED 2
#define D45_PRED 3
#define D135_PRED 4
#define D113_PRED 5
#define D157_PRED 6
#define D203_PRED 7
#define D67_PRED 8
#define SMOOTH_PRED 9
#define SMOOTH_V_PRED 10
#define SMOOTH_H_PRED 11
#define PAETH_PRED 12
#define UV_CFL_PRED 13
#define NEARESTMV 13
#define NEARMV 14
#define GLOBALMV 15
#define NEWMV 16
// Compound ref compound modes
#define NEAREST_NEARESTMV 17
#define NEAR_NEARMV 18
#define NEAREST_NEWMV 19
#define NEW_NEARESTMV 20
#define NEAR_NEWMV 21
#define NEW_NEARMV 22
#define GLOBAL_GLOBALMV 23
#define NEW_NEWMV 24
#define MB_MODE_COUNT 25
#define SINGLE_INTER_MODE_START NEARESTMV
#define SINGLE_INTER_MODE_END NEAREST_NEARESTMV

#define BLOCK_4X4 0
#define BLOCK_4X8 1
#define BLOCK_8X4 2
#define BLOCK_8X8 3
#define BLOCK_8X16 4
#define BLOCK_16X8 5
#define BLOCK_16X16 6
#define BLOCK_16X32 7
#define BLOCK_32X16 8
#define BLOCK_32X32 9
#define BLOCK_32X64 10
#define BLOCK_64X32 11
#define BLOCK_64X64 12
#define BLOCK_64X128 13
#define BLOCK_128X64 14
#define BLOCK_128X128 15
#define BLOCK_4X16 16
#define BLOCK_16X4 17
#define BLOCK_8X32 18
#define BLOCK_32X8 19
#define BLOCK_16X64 20
#define BLOCK_64X16 21

#define SIMPLE_TRANSLATION 0
#define OBMC_CAUSAL 1
#define WARPED_CAUSAL 2
#define MOTION_MODES 3

#define COMPOUND_AVERAGE 0
#define COMPOUND_DISTWTD 1
#define COMPOUND_WEDGE 2
#define COMPOUND_DIFFWTD 3
#define COMPOUND_TYPES 4
#define MASKED_COMPOUND_TYPES 2

#define InterNoSkipFlag 0x2000
#define NeedAboveLut 0x3f7f
#define NeedLeftLut 0x3Ef7
#define NeedRightLut 0x010A
#define NeedBotLut 0x0084
#define NeedAboveLeftLut 0x11ff
#define InterFilterLut 0x25432010

StructuredBuffer<MB_MODE_INFO> buffer_mi : register(t0);
ByteAddressBuffer blocks_indexes : register(t1);
ByteAddressBuffer blocks_index_base : register(t2);
ByteAddressBuffer mi_grid : register(t3);
ByteAddressBuffer intra_iter_grid : register(t4);

RWByteAddressBuffer pred_blocks : register(u0);
RWByteAddressBuffer pred_blocks_warp : register(u1);

cbuffer GenBlockData : register(b0) {
  uint cb_mi_cols;
  uint cb_mi_rows;
  uint cb_mi_stride;
  uint cb_mi_addr_base;
  uint cb_iter_grid_stride;
  uint cb_iter_grid_offset_uv;
  uint cb_iter_grid_stride_uv;
  uint cb_disable_edge_filter;
  uint cb_force_integet_mv;
  int3 cb_reserved;
  int4 cb_wedge_offsets[22];  //??
  int4 cb_dist_wtd[8 * 8];
  int4 cb_lossless_seg[8];
  int4 cb_global_warp[8];
  struct {
    WarpedMotionParams params;
    int pad;
  } cb_wm_params[8];
};

cbuffer GenBlockSRT : register(b1) {
  uint cb_wi_count;
  uint cb_mi_offset;
  uint cb_mi_idx_base;
  uint cb_col_srart;
  uint cb_row_srart;
  uint cb_index_offset;
  uint cb_index_offset_warp;
#ifdef INTER_FAST
  uint cb_index_offset_fast;
#endif
};

#ifdef INTER_FAST
// Test flag -interfast (gen_pred_blocks_fast.hlsl; docs/gpu-av1-decoder.md, "Inter prediction: breakdown and
// fast paths"). The blocks with one reference that are not warped, and the compound blocks that are
// averaged or distance weighted, are listed by the fraction of their motion vectors instead of by their
// type: InterFastLists lists at cb_index_offset_fast of the block map, laid out as av1_mi_push_block()
// counts them (dx/types.h):
//   class * InterSizesAllCommon + size                                         one reference, class 0..3
//   InterFastCompound + (class0 | class1 << 2) * InterSizesAllCommon + size    two references
// The class of a vector as the block record has it (1/16 sample): 1 a fraction in x, 2 a fraction in y.
#define InterFastCompound 96
int inter_fast_class(uint mv) { return (((mv >> 16) & 15) != 0 ? 1 : 0) | ((mv & 15) != 0 ? 2 : 0); }
#endif

int intra_edge_filter_strength(int blk_wh, int d, int type) {
  int strength = 0;
  if (type == 0) {
    if (blk_wh <= 8) {
      if (d >= 56) strength = 1;
    } else if (blk_wh <= 12) {
      if (d >= 40) strength = 1;
    } else if (blk_wh <= 16) {
      if (d >= 40) strength = 1;
    } else if (blk_wh <= 24) {
      if (d >= 8) strength = 1;
      if (d >= 16) strength = 2;
      if (d >= 32) strength = 3;
    } else if (blk_wh <= 32) {
      if (d >= 1) strength = 1;
      if (d >= 4) strength = 2;
      if (d >= 32) strength = 3;
    } else {
      if (d >= 1) strength = 3;
    }
  } else {
    if (blk_wh <= 8) {
      if (d >= 40) strength = 1;
      if (d >= 64) strength = 2;
    } else if (blk_wh <= 16) {
      if (d >= 20) strength = 1;
      if (d >= 48) strength = 2;
    } else if (blk_wh <= 24) {
      if (d >= 4) strength = 3;
    } else {
      if (d >= 1) strength = 3;
    }
  }
  return strength;
}

uint get_mi_index(ByteAddressBuffer grid, int index, uint base) {
  uint addr = grid.Load(index * 8);
  addr -= base;
  return addr / ModeInfoSize;
}

[numthreads(64, 1, 1)] void main(uint3 thread
                                 : SV_DispatchThreadID) {
  if (thread.x >= cb_wi_count) return;

  const int InterBlockSizeIndexLUT[6][6] = {
      // h:        4    8    16    32    64    128
      {0, 1, 2, -1, -1, -1},     // w = 4 (4)
      {3, 4, 5, 6, -1, -1},      // w = 8
      {7, 8, 9, 10, 11, -1},     // w = 16
      {-1, 12, 13, 14, 15, -1},  // w = 32
      {-1, -1, 16, 17, 18, 19},  // w = 64
      {-1, -1, -1, -1, 20, 21}   // w = 128
  };
  const int mi_size_wide_log2[] = {0, 0, 1, 1, 1, 2, 2, 2, 3, 3, 3, 4, 4, 4, 5, 5, 0, 2, 1, 3, 2, 4};
  const int mi_size_high_log2[] = {0, 1, 0, 1, 2, 1, 2, 3, 2, 3, 4, 3, 4, 5, 4, 5, 2, 0, 3, 1, 4, 2};

  const int mi_addr = cb_mi_offset + thread.x;
  MB_MODE_INFO mi = buffer_mi[mi_addr];

  int bsize = mi.block_type & 255;
  const int bw_log = mi_size_wide_log2[bsize];
  const int bh_log = mi_size_high_log2[bsize];
  const int bw = 1 << bw_log;
  const int bh = 1 << bh_log;
  const int bw_log_uv = max(0, bw_log - 1);
  const int bh_log_uv = max(0, bh_log - 1);

  const uint mi_row = mi.mi_row;
  const uint mi_col = mi.mi_col;
  const int is_chroma_ref = ((mi_row & 1) == 0 && (bh & 1) == 1) || ((mi_col & 1) == 0 && (bw & 1) == 1);

  const int ref0 = ((int)mi.block_type << 8) >> 24;
  const int ref1 = ((int)mi.block_type) >> 24;

  const int mode = mi.modes & 255;
  const int is_inter_intra = ref0 > 0 && ref1 == 0 && bsize >= BLOCK_8X8 && bsize <= BLOCK_32X32 &&
                             mode >= SINGLE_INTER_MODE_START && mode < SINGLE_INTER_MODE_END;
  int index_addr = (mi.index_base + cb_mi_idx_base) * 4;
  const int motion_mode = mi.modes >> 24;
  const int is_obmc_left = ref0 > 0 && motion_mode == OBMC_CAUSAL && mi_col > cb_col_srart;
  const int is_obmc_above = ref0 > 0 && motion_mode == OBMC_CAUSAL && mi_row > cb_row_srart;

  if (ref0 > 0) {
    const int is_compound = ref1 > 0;
    const int allow_warp = cb_force_integet_mv == 0 && bw_log > 0 && bh_log > 0;
    const int allow_global_warp = allow_warp && (mode == GLOBALMV || mode == GLOBAL_GLOBALMV);
    const int is_global_warp0 = cb_global_warp[ref0 - 1].x && allow_global_warp;
    const int is_global_warp1 = (is_compound == 0 || allow_global_warp == 0) ? 0 : cb_global_warp[ref1 - 1].x;
    // Bit 16 of the word is wm_params.invalid (0 or 1), which the entropy decoder writes for every
    // WARPED_CAUSAL block. Bits 8..15 are padding of the C struct and stay out of the test.
    const int is_local_warp = motion_mode == WARPED_CAUSAL && (mi.wm_params.type & 0x10000) == 0;
    const int is_luma_warp = (is_local_warp || is_global_warp0) && allow_warp;

    const int no_skip_flag =
        ((mi.tx_info & 0xff00) == 0 && !is_inter_intra && !is_obmc_left && !is_obmc_above) ? InterNoSkipFlag : 0;
    const int block_size_id_y = InterBlockSizeIndexLUT[bw_log][bh_log];
    const int comp_type = mi.interinter_comp.type;
    uint wtd = 0;
    const int wedge_idx = mi.interinter_comp.wedge_sign + mi.interinter_comp.wedge_index * 2;

    if (is_compound) {
      wtd = 0x88;
      if (comp_type == COMPOUND_DISTWTD) {
        wtd = cb_dist_wtd[ref0 - 1 + (ref1 - 1) * 8].x;
      } else if (comp_type == COMPOUND_WEDGE) {
        wtd = cb_wedge_offsets[bsize].x + (wedge_idx << (bw_log + bh_log - 2));
      } else if (comp_type == COMPOUND_DIFFWTD) {
        wtd = mi.interinter_comp.mask_type;
      }
      wtd <<= 17;

      const int is_warp_compound = is_global_warp0 || is_global_warp1;
      const uint filter_type_h =
          (InterFilterLut >> ((((mi.interp_filters >> 16) & 15) << 2) + ((bw_log > 0) << 4))) & 7;
      const uint filter_type_v = (InterFilterLut >> (((mi.interp_filters & 15) << 2) + ((bh_log > 0) << 4))) & 7;
      const int gpu_comp_type = (0x2100 >> (comp_type * 4)) & 15;

      uint4 block;
      block.x = mi_col | (mi_row << 16);
      block.y = ((ref0 - 1) << 2) | (filter_type_h << 5) | (filter_type_v << 9) | ((ref1 - 1) << 14) | wtd |
                no_skip_flag | (gpu_comp_type << 30);

      block.z = (mi.mv[0] << 1) & 0xfffeffff;
      block.w = (mi.mv[1] << 1) & 0xfffeffff;

      const int pass_type = is_warp_compound ? 5 : ((0x3422 >> (comp_type * 4)) & 15);
      int pass_type_index = (pass_type - 1) * InterSizesAllCommon + block_size_id_y;

#ifdef INTER_FAST
      int list_addr = cb_index_offset + pass_type_index;
      if (pass_type == CompoundAvrg)
        list_addr = cb_index_offset_fast + InterFastCompound +
                    (inter_fast_class(block.z) | (inter_fast_class(block.w) << 2)) * InterSizesAllCommon +
                    block_size_id_y;
      int dst_ptr = blocks_index_base.Load(4 * list_addr) + blocks_indexes.Load(index_addr);
#else
      int dst_ptr = blocks_index_base.Load(4 * (cb_index_offset + pass_type_index)) + blocks_indexes.Load(index_addr);
#endif
      index_addr += 4;
      pred_blocks.Store4(dst_ptr * 16, block);
    } else {
      if (is_luma_warp) {
        int dst_ptr =
            blocks_index_base.Load(4 * (cb_index_offset_warp + block_size_id_y)) + blocks_indexes.Load(index_addr);
        index_addr += 4;
        dst_ptr *= 48;
        pred_blocks_warp.Store(dst_ptr, mi_col | (mi_row << 16));
        pred_blocks_warp.Store(dst_ptr + 4, ((ref0 - 1) << 2) | no_skip_flag);

        WarpedMotionParams params;
        if (is_local_warp)
          params = mi.wm_params;
        else
          params = cb_wm_params[ref0 - 1].params;

        pred_blocks_warp.Store4(dst_ptr + 8, params.mat[0]);
        pred_blocks_warp.Store2(dst_ptr + 24, params.mat[1].xy);
        int4 angle32;
        angle32.x = ((int)(params.angles.x << 16)) >> 16;
        angle32.y = ((int)params.angles.x) >> 16;
        angle32.w = ((int)(params.angles.y << 16)) >> 16;
        angle32.z = ((int)params.angles.y) >> 16;
        pred_blocks_warp.Store4(dst_ptr + 32, angle32);
      } else {
        uint4 block;
        block.x = mi_col | (mi_row << 16);
        const uint filter_type_h =
            (InterFilterLut >> ((((mi.interp_filters >> 16) & 15) << 2) + ((bw_log > 0) << 4))) & 7;
        const uint filter_type_v = (InterFilterLut >> (((mi.interp_filters & 15) << 2) + ((bh_log > 0) << 4))) & 7;
        block.y = ((ref0 - 1) << 2) | (filter_type_h << 5) | (filter_type_v << 9) | no_skip_flag;
        block.z = (mi.mv[0] << 1) & 0xfffeffff;
        block.w = 0;

#ifdef INTER_FAST
        int dst_ptr = blocks_index_base.Load(4 * (cb_index_offset_fast + inter_fast_class(block.z) * InterSizesAllCommon +
                                                  block_size_id_y)) +
                      blocks_indexes.Load(index_addr);
#else
        int dst_ptr = blocks_index_base.Load(4 * (cb_index_offset + block_size_id_y)) + blocks_indexes.Load(index_addr);
#endif
        index_addr += 4;
        pred_blocks.Store4(dst_ptr * 16, block);
      }
    }

    if (!is_chroma_ref) {
      const int block_size_id_uv = InterBlockSizeIndexLUT[bw_log_uv][bh_log_uv];
      const int mi_col_uv = mi_col >> 1;
      const int mi_row_uv = mi_row >> 1;
      int sub8x8 = bw_log == 0 || bh_log == 0;

      int mi_addr_above = mi_addr;
      int mi_addr_left = mi_addr;
      int mi_addr_aboveleft = mi_addr;
      if (sub8x8) {
        int dy = bh_log == 0 ? -1 : 0;
        int dx = bw_log == 0 ? -1 : 0;

        mi_addr_left = get_mi_index(mi_grid, mi_col + dx + mi_row * cb_mi_stride, cb_mi_addr_base);
        mi_addr_above = get_mi_index(mi_grid, mi_col + (mi_row + dy) * cb_mi_stride, cb_mi_addr_base);
        mi_addr_aboveleft = get_mi_index(mi_grid, mi_col + dx + (mi_row + dy) * cb_mi_stride, cb_mi_addr_base);

        sub8x8 &= (((int)buffer_mi[mi_addr_left].block_type << 8) >> 24) > 0;
        sub8x8 &= (((int)buffer_mi[mi_addr_above].block_type << 8) >> 24) > 0;
        sub8x8 &= (((int)buffer_mi[mi_addr_aboveleft].block_type << 8) >> 24) > 0;
      }

      if (sub8x8) {
        int x = mi_col & (~1);
        int y = mi_row & (~1);
        const int brows = bh_log == 2 ? 4 : 2;
        const int bcols = bw_log == 2 ? 4 : 2;
        const int bh_flag = bh_log != 0 ? ((brows - 1) << 28) : 0;  // for scale
        const int bw_flag = bw_log != 0 ? ((bcols - 1) << 26) : 0;
        for (int row = 0; row < brows; ++row) {
          int mi_index_1 = row == 0 ? mi_addr_above : mi_addr;
          if (bw_log == 0) {
            const int mi_index_0 = row == 0 ? mi_addr_aboveleft : mi_addr_left;

            const int block_type0 = (int)buffer_mi[mi_index_0].block_type;
            const int block_type1 = (int)buffer_mi[mi_index_1].block_type;

            const int is_compound0 = (block_type0 >> 24) > 0;
            const int is_compound1 = (block_type1 >> 24) > 0;

            const int diff_comp = is_compound0 != is_compound1;
            int type_index0 = Inter2x2ArrOffset + (is_compound0 << diff_comp);
            int type_index1 = Inter2x2ArrOffset + (is_compound1 << diff_comp);

            const int interp_filters0 = buffer_mi[mi_index_0].interp_filters;
            const int interp_filters1 = buffer_mi[mi_index_1].interp_filters;
            const int flags = 1 |                                     // U-plane
                              (is_compound0 == is_compound1) << 25 |  // combo write
                              no_skip_flag | bh_flag;

            int dst_ptr0 =
                blocks_index_base.Load(4 * (cb_index_offset + type_index0)) + blocks_indexes.Load(index_addr);
            index_addr += 4;
            int dst_ptr1 = dst_ptr0 + 1;
            if (diff_comp) {
              dst_ptr1 = blocks_index_base.Load(4 * (cb_index_offset + type_index1)) + blocks_indexes.Load(index_addr);
              index_addr += 4;
            }

            dst_ptr0 *= 16;
            dst_ptr1 *= 16;

            uint filter_type_h =
                (InterFilterLut >> ((((interp_filters0 >> 16) & 15) << 2) + ((bw_log_uv > 0) << 4))) & 7;
            uint filter_type_v = (InterFilterLut >> (((interp_filters0 & 15) << 2) + ((bh_log_uv > 0) << 4))) & 7;
            uint4 block0;
            block0.x = x | ((y + row) << 16);
            block0.y = flags | ((((block_type0 << 8) >> 24) - 1) << 2) | (filter_type_h << 5) | (filter_type_v << 9) |
                       ((((block_type0 >> 24) - 1) & 7) << 14);
            block0.z = buffer_mi[mi_index_0].mv[0];
            block0.w = buffer_mi[mi_index_0].mv[1];

            filter_type_h = (InterFilterLut >> ((((interp_filters1 >> 16) & 15) << 2) + ((bw_log_uv > 0) << 4))) & 7;
            filter_type_v = (InterFilterLut >> (((interp_filters1 & 15) << 2) + ((bh_log_uv > 0) << 4))) & 7;
            uint4 block1;
            block1.x = block0.x + 1;
            block1.y = flags | ((((block_type1 << 8) >> 24) - 1) << 2) | (filter_type_h << 5) | (filter_type_v << 9) |
                       ((((block_type1 >> 24) - 1) & 7) << 14);
            block1.z = buffer_mi[mi_index_1].mv[0];
            block1.w = buffer_mi[mi_index_1].mv[1];

            pred_blocks.Store4(dst_ptr0, block0);
            pred_blocks.Store4(dst_ptr1, block1);
            block0.y ^= 3;
            block1.y ^= 3;
            dst_ptr0 += 32 >> diff_comp;
            dst_ptr1 += 32 >> diff_comp;
            pred_blocks.Store4(dst_ptr0, block0);
            pred_blocks.Store4(dst_ptr1, block1);
          } else {
            const int type_index = Inter2x2ArrOffset + is_compound;
            int dst_addr =
                16 * (blocks_index_base.Load(4 * (cb_index_offset + type_index)) + blocks_indexes.Load(index_addr));
            index_addr += 4;

            const int interp_filters = buffer_mi[mi_index_1].interp_filters;
            uint filter_type_h =
                (InterFilterLut >> ((((interp_filters >> 16) & 15) << 2) + ((bw_log_uv > 0) << 4))) & 7;
            uint filter_type_v = (InterFilterLut >> (((interp_filters & 15) << 2) + ((bh_log_uv > 0) << 4))) & 7;
            const int block_type = (int)buffer_mi[mi_index_1].block_type;

            uint mode_base = ((((block_type << 8) >> 24) - 1) << 2) | (filter_type_h << 5) | (filter_type_v << 9) |
                             ((((block_type >> 24) - 1) & 7) << 14) | (1 << 25) | no_skip_flag | bw_flag;

            uint4 block;
            block.z = buffer_mi[mi_index_1].mv[0];
            block.w = buffer_mi[mi_index_1].mv[1];

            for (int p = 1; p < 3; ++p) {
              for (int col = 0; col < bcols; ++col) {
                block.x = (x + col) | ((y + row) << 16);
                block.y = mode_base | p;
                pred_blocks.Store4(dst_addr, block);
                dst_addr += 16;
              }
            }
          }
        }
      } else  //! sub8x8
      {
        const uint filter_type_h =
            (InterFilterLut >> ((((mi.interp_filters >> 16) & 15) << 2) + ((bw_log_uv > 0) << 4))) & 7;
        const uint filter_type_v = (InterFilterLut >> (((mi.interp_filters & 15) << 2) + ((bh_log_uv > 0) << 4))) & 7;
        if (is_compound) {
          if (comp_type == COMPOUND_WEDGE) {
            wtd = cb_wedge_offsets[bsize].y + (wedge_idx << max(0, bw_log_uv + bh_log_uv - 2));
            wtd <<= 17;
          }

          const int is_warp_compound = (is_global_warp0 || is_global_warp1) && bw_log > 1 && bh_log > 1;
          const int gpu_comp_type = (0x3100 >> (comp_type * 4)) & 15;

          uint4 block;
          block.x = mi_col_uv | (mi_row_uv << 16);
          block.y = 1 | ((ref0 - 1) << 2) | (filter_type_h << 5) | (filter_type_v << 9) | ((ref1 - 1) << 14) | wtd |
                    no_skip_flag | (gpu_comp_type << 30);
          block.z = mi.mv[0];
          block.w = mi.mv[1];
          const int pass_type =
              is_warp_compound ? (comp_type == COMPOUND_DIFFWTD ? 7 : 5) : ((0x6422 >> (comp_type * 4)) & 15);
          int pass_type_index = (pass_type - 1) * InterSizesAllCommon + block_size_id_uv;
#ifdef INTER_FAST
          int list_addr = cb_index_offset + pass_type_index;
          if (pass_type == CompoundAvrg)
            list_addr = cb_index_offset_fast + InterFastCompound +
                        (inter_fast_class(block.z) | (inter_fast_class(block.w) << 2)) * InterSizesAllCommon +
                        block_size_id_uv;
          int dst_ptr = blocks_index_base.Load(4 * list_addr) + blocks_indexes.Load(index_addr);
#else
          int dst_ptr =
              blocks_index_base.Load(4 * (cb_index_offset + pass_type_index)) + blocks_indexes.Load(index_addr);
#endif
          index_addr += 4;
          dst_ptr *= 16;
          pred_blocks.Store4(dst_ptr, block);
          block.y ^= 3;
          pred_blocks.Store4(dst_ptr + 16, block);
        } else {
          const int is_chroma_warp = is_luma_warp && bw_log >= 2 && bh_log >= 2;
          if (is_chroma_warp) {
            int dst_ptr =
                blocks_index_base.Load(4 * (cb_index_offset_warp + block_size_id_uv)) + blocks_indexes.Load(index_addr);
            index_addr += 4;
            dst_ptr *= 48;

            WarpedMotionParams params;
            if (is_local_warp)
              params = mi.wm_params;
            else
              params = cb_wm_params[ref0 - 1].params;

            int4 angle32;
            angle32.x = ((int)(params.angles.x << 16)) >> 16;
            angle32.y = ((int)params.angles.x) >> 16;
            angle32.w = ((int)(params.angles.y << 16)) >> 16;
            angle32.z = ((int)params.angles.y) >> 16;

            pred_blocks_warp.Store(dst_ptr, mi_col_uv | (mi_row_uv << 16));
            pred_blocks_warp.Store(dst_ptr + 4, 1 | ((ref0 - 1) << 2) | no_skip_flag);
            pred_blocks_warp.Store4(dst_ptr + 8, params.mat[0]);
            pred_blocks_warp.Store2(dst_ptr + 24, params.mat[1].xy);
            pred_blocks_warp.Store4(dst_ptr + 32, angle32);
            dst_ptr += 48;
            pred_blocks_warp.Store(dst_ptr, mi_col_uv | (mi_row_uv << 16));
            pred_blocks_warp.Store(dst_ptr + 4, 2 | ((ref0 - 1) << 2) | no_skip_flag);
            pred_blocks_warp.Store4(dst_ptr + 8, params.mat[0]);
            pred_blocks_warp.Store2(dst_ptr + 24, params.mat[1].xy);
            pred_blocks_warp.Store4(dst_ptr + 32, angle32);

          } else {
            uint4 block;
            block.x = mi_col_uv | (mi_row_uv << 16);
            block.y = 1 | ((ref0 - 1) << 2) | (filter_type_h << 5) | (filter_type_v << 9) | no_skip_flag;
            block.z = mi.mv[0];
            block.w = 0;

#ifdef INTER_FAST
            int dst_ptr = blocks_index_base.Load(4 * (cb_index_offset_fast +
                                                      inter_fast_class(block.z) * InterSizesAllCommon + block_size_id_uv)) +
                          blocks_indexes.Load(index_addr);
#else
            int dst_ptr =
                blocks_index_base.Load(4 * (cb_index_offset + block_size_id_uv)) + blocks_indexes.Load(index_addr);
#endif
            index_addr += 4;
            dst_ptr *= 16;
            pred_blocks.Store4(dst_ptr, block);
            dst_ptr += 16;
            block.y ^= 3;
            pred_blocks.Store4(dst_ptr, block);
          }
        }
      }
    }

    if (is_obmc_above) {
      const int x_mis = min(bw, cb_mi_cols - mi_col);
      int h = bh_log > 4 ? 3 : (bh_log - 1);
      int huv = h == 0 ? 0 : h - 1;
      int obmc_chroma = bsize > BLOCK_16X8 && bsize != BLOCK_4X16 && bsize != BLOCK_16X4;
      int count = 0;
      for (int col = 0; col < x_mis && count < min(bw_log, 4);) {
        int mi_addr_above = get_mi_index(mi_grid, mi_col + col + (mi_row - 1) * cb_mi_stride, cb_mi_addr_base);
        int w = mi_size_wide_log2[buffer_mi[mi_addr_above].block_type & 255];
        if (w == 0) {
          w = 1;
          mi_addr_above = get_mi_index(mi_grid, mi_col + col + 1 + (mi_row - 1) * cb_mi_stride, cb_mi_addr_base);
        }
        if (w > bw_log) w = bw_log;

        uint above_ref = ((int)buffer_mi[mi_addr_above].block_type << 8) >> 24;
        if (above_ref > 0) {
          count += 1 + (w == 5);
          const int filters = buffer_mi[mi_addr_above].interp_filters;

          uint filter_type_h = (InterFilterLut >> ((((filters >> 16) & 15) << 2) + ((w > 0) << 4))) & 7;
          uint filter_type_v = (InterFilterLut >> (((filters & 15) << 2) + ((h > 0) << 4))) & 7;

          int4 block;
          block.x = (mi_col + col) | (mi_row << 16);
          block.y = 0 | ((above_ref - 1) << 2) | (filter_type_h << 5) | (filter_type_v << 9) | ((1 << h) << 17);
          block.z = (buffer_mi[mi_addr_above].mv[0] << 1) & 0xfffeffff;
          block.w = 0;

          int type_index = (ObmcAbove - 1) * InterSizesAllCommon + ((w << 2) | h);  // InterBlockSizeIndexLUT[w][h];
          int dst_ptr = blocks_index_base.Load(4 * (cb_index_offset + type_index)) + blocks_indexes.Load(index_addr);
          index_addr += 4;
          pred_blocks.Store4(dst_ptr * 16, block);

          if (obmc_chroma) {
            filter_type_h = (InterFilterLut >> ((((filters >> 16) & 15) << 2) + ((w > 1) << 4))) & 7;
            filter_type_v = (InterFilterLut >> (((filters & 15) << 2) + ((huv > 0) << 4))) & 7;
            block.x = ((mi_col + col) >> 1) | ((mi_row >> 1) << 16);
            block.y = 1 | ((above_ref - 1) << 2) | (filter_type_h << 5) | (filter_type_v << 9) |
                      (((0x84210 >> (h * 4)) & 15) << 17);
            block.z = buffer_mi[mi_addr_above].mv[0];

            type_index = (ObmcAbove - 1) * InterSizesAllCommon + (((w - 1) << 2) | huv);
            dst_ptr = blocks_index_base.Load(4 * (cb_index_offset + type_index)) + blocks_indexes.Load(index_addr);
            index_addr += 4;
            dst_ptr *= 16;
            pred_blocks.Store4(dst_ptr, block);
            block.y ^= 3;
            dst_ptr += 16;
            pred_blocks.Store4(dst_ptr, block);
          }
        }
        col += 1 << w;
      }
    }

    if (is_obmc_left) {
      const int y_mis = min(bh, cb_mi_rows - mi_row);
      int w = bw_log > 4 ? 3 : (bw_log - 1);
      int wuv = w == 0 ? 0 : w - 1;
      int count = 0;
      for (int row = 0; row < y_mis && count < min(bh_log, 4);) {
        int mi_addr_left = get_mi_index(mi_grid, mi_col - 1 + (mi_row + row) * cb_mi_stride, cb_mi_addr_base);
        int h = mi_size_high_log2[buffer_mi[mi_addr_left].block_type & 255];
        if (h == 0) {
          h = 1;
          // left += xd->mi_stride;
          mi_addr_left = get_mi_index(mi_grid, mi_col - 1 + (mi_row + row + 1) * cb_mi_stride, cb_mi_addr_base);
        }
        if (h > bh_log) h = bh_log;

        uint left_ref = ((int)buffer_mi[mi_addr_left].block_type << 8) >> 24;
        if (left_ref > 0) {
          count += 1 + (h == 5);

          const int filters = buffer_mi[mi_addr_left].interp_filters;
          uint filter_type_h = (InterFilterLut >> ((((filters >> 16) & 15) << 2) + ((w > 0) << 4))) & 7;
          uint filter_type_v = (InterFilterLut >> (((filters & 15) << 2) + ((h > 0) << 4))) & 7;

          int4 block;
          block.x = mi_col | ((mi_row + row) << 16);
          block.y = 0 | ((left_ref - 1) << 2) | (filter_type_h << 5) | (filter_type_v << 9) | ((1 << w) << 17);
          block.z = (buffer_mi[mi_addr_left].mv[0] << 1) & 0xfffeffff;
          block.w = 0;

          int type_index = (ObmcLeft - 1) * InterSizesAllCommon + ((h << 2) | w);  // InterBlockSizeIndexLUT[w][h];
          int dst_ptr = blocks_index_base.Load(4 * (cb_index_offset + type_index)) + blocks_indexes.Load(index_addr);
          index_addr += 4;
          pred_blocks.Store4(dst_ptr * 16, block);
          filter_type_h = (InterFilterLut >> ((((filters >> 16) & 15) << 2) + ((wuv > 0) << 4))) & 7;
          filter_type_v = (InterFilterLut >> (((filters & 15) << 2) + ((h > 1) << 4))) & 7;
          block.x = (mi_col >> 1) | (((mi_row + row) >> 1) << 16);
          block.y = 1 | ((left_ref - 1) << 2) | (filter_type_h << 5) | (filter_type_v << 9) |
                    (((0x84210 >> (w * 4)) & 15) << 17);
          block.z = buffer_mi[mi_addr_left].mv[0];

          type_index = (ObmcLeft - 1) * InterSizesAllCommon + (((h - 1) << 2) | wuv);
          dst_ptr = blocks_index_base.Load(4 * (cb_index_offset + type_index)) + blocks_indexes.Load(index_addr);
          index_addr += 4;
          dst_ptr *= 16;
          pred_blocks.Store4(dst_ptr, block);
          block.y ^= 3;
          dst_ptr += 16;
          pred_blocks.Store4(dst_ptr, block);
        }
        row += 1 << h;
      }
    }
  }

  const int y_use_palette = (mi.palette_mode_info.sizes & 0xffff) != 0;
  const int uv_use_palette = (mi.palette_mode_info.sizes & 0xffff0000) != 0;

  if (ref0 <= 0 || is_inter_intra) {
    const int tx_size_wide_log2[] = {0, 1, 2, 3, 4, 0, 1, 1, 2, 2, 3, 3, 4, 0, 2, 1, 3, 2, 4};
    const int tx_size_high_log2[] = {0, 1, 2, 3, 4, 1, 0, 2, 1, 3, 2, 4, 3, 2, 0, 3, 1, 4, 2};
    const int mode_to_angle_map[] = {
        0, 90, 180, 45, 135, 113, 157, 203, 67,
    };

    const int disable_edge_filter = cb_disable_edge_filter;
    const int intra_mode_flags = mi.intra_mode_flags;
    const int is_intrabc = (intra_mode_flags & 0x100) != 0;
    const int interintra_mode = (mi.modes >> 16) & 255;

    uint tx_info = mi.tx_info;
    uint tx_size = tx_info & 255;

    int txw = tx_size_wide_log2[tx_size];
    int txh = tx_size_high_log2[tx_size];
    const int tx_uv_add = is_intrabc || is_inter_intra;
    int txw_uv = min(bw_log_uv, 3 + tx_uv_add);
    int txh_uv = min(bh_log_uv, 3 + tx_uv_add);
    if (cb_lossless_seg[(tx_info >> 24) & 7].x && !tx_uv_add) {
      txw = 0;
      txh = 0;
      txw_uv = 0;
      txh_uv = 0;
    }

    txw = is_intrabc ? min(bw_log, 4) : is_inter_intra ? bw_log : txw;
    txh = is_intrabc ? min(bh_log, 4) : is_inter_intra ? bh_log : txh;

    const int max_cnt_x = (cb_mi_cols - mi_col + (1 << txw) - 1) >> txw;
    const int max_cnt_y = (cb_mi_rows - mi_row + (1 << txh) - 1) >> txh;
    const int unit_x_log = bw_log == 5 && !is_intrabc;
    const int unit_y_log = bh_log == 5 && !is_intrabc;

    int cnt_y = 1 << (bh_log - txh);
    int cnt_x = 1 << (bw_log - txw);
    const int cfl_max_x = (mi_col + (min(cnt_x, max_cnt_x) << txw)) << 2;
    const int cfl_max_y = (mi_row + (min(cnt_y, max_cnt_y) << txh)) << 2;

    if (!y_use_palette) {
      const int mode1 = (is_inter_intra ? (0x9210 >> (interintra_mode * 4)) : mi.modes) & 15;
      int need_above = (NeedAboveLut >> mode1) & 1;
      int need_left = (NeedLeftLut >> mode1) & 1;

      // FILTER_INTRA_MODE_INFO of blockd.h is two bytes (filter_intra_mode, use_filter_intra). The upper 16
      // bits of the word they are in are padding of the C struct: the entropy decoder never writes them, and
      // the memory of the mode info pool is not cleared, so they hold what the memory held before (a decoder
      // destroyed earlier in the process, for one). Only the byte of use_filter_intra counts.
      const int use_filter = (mi.filter_intra_mode_info >> 8) & 255;

      const int mode_gpu = is_intrabc ? (12 << 6) : use_filter ? (13 << 6) : mode1 ? ((mode1 - 1) << 6) : (14 << 6);

      int is_dir = mode1 >= V_PRED && mode1 <= D67_PRED;

      int mode_flags_base = txw | (((tx_info & 0xff00) == 0) << 5) | mode_gpu;

      int dir_above_filter = 0;
      int dir_left_filter = 0;

      if (is_dir) {
        int upsample_above = 0;
        int upsample_left = 0;
        int angle_delta = (intra_mode_flags << 8) >> 24;
        int angle = mode_to_angle_map[mode1] + angle_delta * 3;
        const int mode_angle = angle_delta + 3;
        if (!disable_edge_filter) {
          int filt_type = 0;
          if (mi_row > cb_row_srart) {
            const int above_idx = get_mi_index(mi_grid, mi_col + (mi_row - 1) * cb_mi_stride, cb_mi_addr_base);
            const int m = buffer_mi[above_idx].modes & 255;
            filt_type = m == SMOOTH_PRED || m == SMOOTH_V_PRED || m == SMOOTH_H_PRED;
          }
          if (mi_col > cb_col_srart) {
            const int left_idx = get_mi_index(mi_grid, mi_col - 1 + mi_row * cb_mi_stride, cb_mi_addr_base);
            const int m = buffer_mi[left_idx].modes & 255;
            filt_type |= m == SMOOTH_PRED || m == SMOOTH_V_PRED || m == SMOOTH_H_PRED;
          }
          int d90 = abs(angle - 90);
          int d180 = abs(angle - 180);
          int blk_wh = (4 << txw) + (4 << txh);
          upsample_above = d90 != 0 && d90 < 40 && blk_wh <= (16 >> filt_type);
          upsample_left = d180 != 0 && d180 < 40 && blk_wh <= (16 >> filt_type);
          dir_above_filter = intra_edge_filter_strength(blk_wh, d90, filt_type);
          dir_left_filter = intra_edge_filter_strength(blk_wh, d180, filt_type);
        }
        mode_flags_base |= (upsample_above << 22) | (upsample_left << 23) | (mode_angle << 28);
      }

      mode_flags_base |= is_inter_intra << 31;

      int mode_info0 = 0;
      if (is_inter_intra) {
        const int w_idx = mi.interintra_wedge_sign + mi.interintra_wedge_index * 2;
        const int w_ofs = cb_wedge_offsets[bsize].x;
        const int w_sz = 1 << max(0, bw_log + bh_log - 2);
        mode_info0 = w_ofs + w_sz * ((intra_mode_flags & 1) ? w_idx : (32 + interintra_mode));
      } else if (is_intrabc) {
        mode_info0 = (mi.mv[0] << 1) & 0xfffeffff;
        need_left = 0;
        need_above = 0;
      } else if (use_filter) {
        mode_info0 = txh | ((mi.filter_intra_mode_info & 255) << 4);
      }

      const int type_size = txw + txh;
      const int type_idx_base = use_filter ? IntraBlockOffset : (IntraBlockOffset - 1 - type_size);
      cnt_x >>= unit_x_log;
      cnt_y >>= unit_y_log;

      for (int unit_y = 0; unit_y <= unit_y_log; ++unit_y) {
        for (int unit_x = 0; unit_x <= unit_x_log; ++unit_x) {
          const int x_start = unit_x * cnt_x;
          const int x_end = min(x_start + cnt_x, max_cnt_x);
          const int y_start = unit_y * cnt_y;
          const int y_end = min(y_start + cnt_y, max_cnt_y);
          for (int y = y_start; y < y_end; ++y) {
            for (int x = x_start; x < x_end; ++x) {
              const int col = mi_col + (x << txw);
              const int row = mi_row + (y << txh);
              const int subblk_w = 1 << txw;
              const int subblk_h = 1 << txh;
              const int have_top = y || (mi_row > cb_row_srart);
              const int have_left = x || (mi_col > cb_col_srart);
              uint block_index = blocks_indexes.Load(index_addr);
              index_addr += 4;

              int above_available = have_top;
              if (need_above) {
                const int xr = cb_mi_cols - col - subblk_w;
                int have_top_right = block_index & 1;
                above_available =
                    (have_top ? min(subblk_w, subblk_w + xr) : 0) + (have_top_right ? min(subblk_w, xr) : 0);
              }

              int left_available = have_left;
              if (need_left) {
                const int yd = cb_mi_rows - row - subblk_h;
                int have_bottom_left = block_index & 2;
                left_available =
                    (have_left ? min(subblk_h, subblk_h + yd) : 0) + (have_bottom_left ? min(subblk_h, yd) : 0);
              }

              int iter_grid_stride = cb_iter_grid_stride;
              int iter = intra_iter_grid.Load((col + subblk_w + (row + 1) * iter_grid_stride) * 4);
              const int type_index = iter * IntraTypeCount + type_idx_base;
              const int dst_ptr = blocks_index_base.Load(4 * (cb_index_offset + type_index)) + (block_index >> 2);

              uint4 block;
              block.x = col | (row << 16);
              block.y = mode_flags_base | (above_available << 10) | (left_available << 16) |
                        (above_available ? (dir_above_filter << 24) : 0) |
                        (left_available ? (dir_left_filter << 26) : 0);
              block.z = mode_info0;
              block.w = 0;
              pred_blocks.Store4(dst_ptr * 16, block);
            }
          }
        }
      }
    }

    if (!uv_use_palette && !is_chroma_ref) {
      const int mi_col_uv = mi_col >> 1;
      const int mi_row_uv = mi_row >> 1;
      const int mode1 = (is_inter_intra ? (0x9210 >> (interintra_mode * 4)) : (mi.modes >> 8)) & 15;

      int need_above = (NeedAboveLut >> mode1) & 1;
      int need_left = (NeedLeftLut >> mode1) & 1;

      const int mode_gpu =
          is_intrabc ? (12 << 6) : mode1 == UV_CFL_PRED ? (15 << 6) : mode1 ? ((mode1 - 1) << 6) : (14 << 6);

      int is_dir = mode1 >= V_PRED && mode1 <= D67_PRED;

      int mode_flags_base = txw_uv | (((tx_info & 0xff00) == 0) << 5) | mode_gpu;

      int dir_above_filter = 0;
      int dir_left_filter = 0;

      const uint mi_col1 = mi_col & ~1;
      const uint mi_row1 = mi_row & ~1;
      if (is_dir) {
        int upsample_above = 0;
        int upsample_left = 0;
        int angle_delta = intra_mode_flags >> 24;
        int angle = mode_to_angle_map[mode1] + angle_delta * 3;
        const int mode_angle = angle_delta + 3;
        if (!disable_edge_filter) {
          int filt_type = 0;
          const int mi_base = mi_col1 + mi_row1 * cb_mi_stride;
          if (mi_row1 > cb_row_srart) {
            const int above_idx = get_mi_index(mi_grid, mi_base + 1 - cb_mi_stride, cb_mi_addr_base);
            const int m = (buffer_mi[above_idx].modes >> 8) & 255;
            filt_type = (m == SMOOTH_PRED || m == SMOOTH_V_PRED || m == SMOOTH_H_PRED) &&
                        (((int)buffer_mi[above_idx].block_type << 8) >> 24) <= 0 &&
                        (buffer_mi[above_idx].intra_mode_flags & 0x100) == 0;
          }
          if (mi_col1 > cb_col_srart) {
            const int left_idx = get_mi_index(mi_grid, mi_base - 1 + cb_mi_stride, cb_mi_addr_base);
            const int m = (buffer_mi[left_idx].modes >> 8) & 255;
            filt_type |= (m == SMOOTH_PRED || m == SMOOTH_V_PRED || m == SMOOTH_H_PRED) &&
                         (((int)buffer_mi[left_idx].block_type << 8) >> 24) <= 0 &&
                         (buffer_mi[left_idx].intra_mode_flags & 0x100) == 0;
          }
          int d90 = abs(angle - 90);
          int d180 = abs(angle - 180);
          int blk_wh = (4 << txw_uv) + (4 << txh_uv);
          upsample_above = d90 != 0 && d90 < 40 && blk_wh <= (16 >> filt_type);
          upsample_left = d180 != 0 && d180 < 40 && blk_wh <= (16 >> filt_type);
          dir_above_filter = intra_edge_filter_strength(blk_wh, d90, filt_type);
          dir_left_filter = intra_edge_filter_strength(blk_wh, d180, filt_type);
        }
        mode_flags_base |= (upsample_above << 22) | (upsample_left << 23) | (mode_angle << 28);
      }

      mode_flags_base |= is_inter_intra << 31;

      int mode_u = 1 << 3;  // plane
      int mode_v = 2 << 3;  // plane
      int mode_info0 = 0;
      if (mode1 == UV_CFL_PRED) {
        int sign_u = ((mi.cfl_alpha_signs + 1) * 11) >> 5;   // CFL_SIGN_U(cfl_alpha_signs);
        int sign_v = (mi.cfl_alpha_signs + 1) - 3 * sign_u;  // CFL_SIGN_V(cfl_alpha_signs);
        int idx_u = (sign_u == 2) ? (mi.cfl_alpha_idx >> 4) + 1 : (sign_u == 1) ? -(mi.cfl_alpha_idx >> 4) - 1 : 0;
        int idx_v = (sign_v == 2) ? (mi.cfl_alpha_idx & 15) + 1 : (sign_v == 1) ? -(mi.cfl_alpha_idx & 15) - 1 : 0;
        mode_u |= (idx_u + 16) << 22;
        mode_v |= (idx_v + 16) << 22;
        mode_info0 = cfl_max_x | (cfl_max_y << 16);
      }

      if (is_inter_intra) {
        const int w_idx = mi.interintra_wedge_sign + mi.interintra_wedge_index * 2;
        const int w_ofs = cb_wedge_offsets[bsize].y;
        const int w_sz = 1 << max(0, bw_log_uv + bh_log_uv - 2);
        mode_info0 = w_ofs + w_sz * ((intra_mode_flags & 1) ? w_idx : (32 + interintra_mode));
      } else if (is_intrabc) {
        mode_info0 = mi.mv[0];
        need_left = 0;
        need_above = 0;
      }

      const int type_idx_base = IntraBlockOffset - 1 - txw_uv - txh_uv;
      const int cnt_y_uv = 1 << (bh_log_uv - txh_uv - unit_y_log);
      const int cnt_x_uv = 1 << (bw_log_uv - txw_uv - unit_x_log);
      for (int unit_y = 0; unit_y <= unit_y_log; ++unit_y) {
        for (int unit_x = 0; unit_x <= unit_x_log; ++unit_x) {
          for (int suby = 0; suby < cnt_y_uv; ++suby) {
            for (int subx = 0; subx < cnt_x_uv; ++subx) {
              const int x = subx + unit_x * cnt_x_uv;
              const int y = suby + unit_y * cnt_y_uv;
              const int col = mi_col_uv + (x << txw_uv);
              const int row = mi_row_uv + (y << txh_uv);
              const int subblk_w = 1 << txw_uv;
              const int subblk_h = 1 << txh_uv;
              const int have_top = y || (mi_row1 > cb_row_srart);
              const int have_left = x || (mi_col1 > cb_col_srart);

              uint block_index = blocks_indexes.Load(index_addr);
              index_addr += 4;

              int above_available = have_top;
              if (need_above) {
                const int xr = ((cb_mi_cols - mi_col - bw) + (2 << bw_log_uv) - ((x + 1) << (txw_uv + 1))) >> 1;
                int have_top_right = block_index & 1;
                above_available =
                    (have_top ? min(subblk_w, subblk_w + xr) : 0) + (have_top_right ? min(subblk_w, xr) : 0);
              }

              int left_available = have_left;
              if (need_left) {
                const int yd = ((cb_mi_rows - mi_row - bh) + (2 << bh_log_uv) - ((y + 1) << (txh_uv + 1))) >> 1;
                int have_bottom_left = block_index & 2;
                left_available =
                    (have_left ? min(subblk_h, subblk_h + yd) : 0) + (have_bottom_left ? min(subblk_h, yd) : 0);
              }

              int iter_grid_offset = cb_iter_grid_offset_uv;
              int iter_grid_stride = cb_iter_grid_stride_uv;
              int iter = intra_iter_grid.Load((iter_grid_offset + col + subblk_w + (row + 1) * iter_grid_stride) * 4);
              const int type_index = iter * IntraTypeCount + type_idx_base;
              int dst_ptr = blocks_index_base.Load(4 * (cb_index_offset + type_index)) + (block_index >> 2);
              dst_ptr *= 16;

              uint uv_mode = mode_flags_base | (above_available << 10) | (left_available << 16) |
                             (above_available ? (dir_above_filter << 24) : 0) |
                             (left_available ? (dir_left_filter << 26) : 0);
              uint4 block;
              block.x = col | (row << 16);
              block.y = uv_mode | mode_u;
              block.z = mode_info0;
              block.w = 0;
              pred_blocks.Store4(dst_ptr, block);

              dst_ptr += 16;
              block.y = uv_mode | mode_v;
              pred_blocks.Store4(dst_ptr, block);
            }
          }
        }
      }
    }
  }

  const int do_recon = (mi.tx_info & 0xff00) == 0;
  const int inter_recon = do_recon && (is_obmc_above || is_obmc_left);
  if (y_use_palette || inter_recon) {
    const int type_index = ReconBlockOffset + bw_log + 6 * bh_log;
    int dst_ptr = blocks_index_base.Load(4 * (cb_index_offset + type_index)) + blocks_indexes.Load(index_addr);

    index_addr += 4;
    dst_ptr *= 16;

    uint4 block;
    block.x = mi_col | (mi_row << 16);
    block.y = (do_recon << 2) | (y_use_palette << 3);
    block.z = 0;
    block.w = 0;
    pred_blocks.Store4(dst_ptr, block);
  }
  if ((uv_use_palette || inter_recon) && !is_chroma_ref) {
    const int type_index = ReconBlockOffset + bw_log_uv + 6 * bh_log_uv;
    int dst_ptr = blocks_index_base.Load(4 * (cb_index_offset + type_index)) + blocks_indexes.Load(index_addr);

    index_addr += 4;
    dst_ptr *= 16;

    uint4 block;
    block.x = (mi_col >> 1) | ((mi_row >> 1) << 16);
    block.y = 1 | (do_recon << 2) | (uv_use_palette << 3);
    block.z = 0;
    block.w = 0;
    pred_blocks.Store4(dst_ptr, block);
    dst_ptr += 16;
    block.y = 2 | (do_recon << 2) | (uv_use_palette << 3);
    pred_blocks.Store4(dst_ptr, block);
  }
}
