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

// The global warp constants use this layout.
struct WarpedMotionParams {
  int4 mat[2];
  int2 angles;  // x: alpha (bits 0..15), beta (16..31); y: gamma, delta
  int type;
};

// The sink clears each record before filling it. Packed fields are read with masks or shifts;
// byte 0 is bits 0..7. The layout tests of rav1g-sys check this against records.rs.
struct MB_MODE_INFO {
  int4 warp_mat0;   // local warp, coefficients 0..3
  int2 warp_mat1;   // coefficients 4 and 5
  int2 warp_angles; // x: alpha and beta; y: gamma and delta, all signed 16-bit
  int mv[2];
  uint mi_position; // low half: mi_row, high half: mi_col
  uint index_base;

  uint block_type;       // sb_type, partition, ref_frame[0], ref_frame[1] (signed)
  uint modes;            // mode, uv_mode, interintra_mode, motion_mode
  uint intra_mode_flags; // use_wedge_interintra, use_intrabc, angle_delta[2] (signed)
  uint tx_info;          // tx_size, skip, cdef_strength (signed), segment_id
  uint inter_tx_size[4]; // 16 bytes, a transform size each, for inter and intrabc blocks
  uint delta_lf;         // four signed loop filter deltas
  uint intra_params;     // delta_lf_from_base (signed), cfl_alpha_idx, cfl_alpha_signs, interintra_wedge_index
  uint palette_filter_flags; // palette_size[2], filter_intra_mode, flags
  uint compound_filters;     // compound_type, wedge_index, vertical filter, horizontal filter
};

#define ModeInfoSize 96
#define MI_USE_FILTER_INTRA 1
#define MI_WARP_INVALID 2
#define MI_MASK_TYPE 4
#define MI_WEDGE_SIGN 8

// Byte offsets of the aligned words read by mi_grid_build and cdef_lists.
#define MI_POSITION_WORD 40
#define SB_TYPE_WORD 48
#define TX_INFO_WORD 60
