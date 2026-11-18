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

// The 224-byte record used by the C++ equality reference. The legacy fast shader is built
// for the shader comparison. The decoder dispatches gen_pred_blocks_fast_packed.
struct INTERINTER_COMPOUND_DATA {
  int type;
  int mask_type;
  int wedge_index;
  int wedge_sign;
};

struct WarpedMotionParams {
  int4 mat[2];
  int2 angles;  // x: alpha (bits 0..15), beta (16..31); y: gamma, delta
  // byte 0 wmtype, byte 1 PADDING, bytes 2..3 invalid (0 or 1). Only bit 16 (invalid) is read.
  int type;
};

struct PALETTE_MODE_INFO {
  int colors[12];  // TODO remove this?
  uint sizes;      // bits 0..15 palette_size[0] (luma), bits 16..31 palette_size[1] (chroma)
};

struct MB_MODE_INFO {
  PALETTE_MODE_INFO palette_mode_info;
  WarpedMotionParams wm_params;
  INTERINTER_COMPOUND_DATA interinter_comp;
  // byte 0 filter_intra_mode, byte 1 use_filter_intra, bytes 2..3 PADDING: mask both reads to their byte.
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

  uint comp_idx;    // bits 0..15 comp_group_idx, bits 16..31 compound_idx (no shader reads it)
  uint block_type;  // byte 0 sb_type, byte 1 partition, byte 2 ref_frame[0], byte 3 ref_frame[1] (signed)
  uint modes;       // byte 0 mode, byte 1 uv_mode, byte 2 interintra_mode, byte 3 motion_mode
  //    uint txk_type[16];
  uint intra_mode_flags;  // byte 0 use_wedge_interintra, byte 1 use_intrabc, bytes 2 and 3 angle_delta[2] (signed)
  // byte 0 tx_size, byte 1 skip, byte 2 skip_mode (not written in intra frames: do not read it), byte 3 segment_id
  uint tx_info;
  uint inter_tx_size[4];  // 16 bytes, one transform size each; written for inter and intrabc blocks
  // byte 0 seg_id_predicted, byte 1 cdef_strength, byte 2 ref_mv_idx, byte 3 reserved (never written). No
  // shader reads this word.
  uint seg_cdef_flags;
};  // sizeof(MB_MODE_INFO)==224

#define ModeInfoSize 224
