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

struct INTERINTER_COMPOUND_DATA {
  int type;
  int mask_type;
  int wedge_index;
  int wedge_sign;
};

struct WarpedMotionParams {
  int4 mat[2];
  int2 angles;
  int type;
};

struct PALETTE_MODE_INFO {
  int colors[12];  // TODO remove this?
  uint sizes;
};

struct MB_MODE_INFO {
  PALETTE_MODE_INFO palette_mode_info;
  WarpedMotionParams wm_params;
  INTERINTER_COMPOUND_DATA interinter_comp;
  uint filter_intra_mode_info;
  int mv[2];
  uint interp_filters;
  // TODO(debargha): Consolidate these flags
  int interintra_wedge_index;
  int interintra_wedge_sign;
  int current_qindex;
  int delta_lf_from_base;
  int delta_lf[4];

  int mi_row;
  int mi_col;
  uint index_base;

  int num_proj_ref;
  // Index of the alpha Cb and alpha Cr combination
  int cfl_alpha_idx;
  // Joint sign of alpha Cb and alpha Cr
  int cfl_alpha_signs;

  uint comp_idx;
  uint block_type;
  uint modes;
  //    uint txk_type[16];
  uint intra_mode_flags;
  uint tx_info;
  uint inter_tx_size[4];
  uint seg_cdef_flags;
};  // sizeof(MB_MODE_INFO)==224

#define ModeInfoSize 224