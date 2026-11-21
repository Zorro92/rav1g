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

#define luma_subblock_size_y 32
#define luma_subblock_size_x 32
#define chroma_subblock_size_y 16
#define chroma_subblock_size_x 16
#define min_luma_legal_range 16
#define max_luma_legal_range 235
#define min_chroma_legal_range 16
#define max_chroma_legal_range 240

struct GrainParams {
  // This structure is compared element-by-element in the function
  // av1_check_grain_params_equiv: this function must be updated if any changes
  // are made to this structure.
  int apply_grain;
  int update_parameters;
  int num_y_points;   // value: 0..14
  int num_cb_points;  // value: 0..10

  int num_cr_points;   // value: 0..10
  int scaling_shift;   // values : 8..11
  int ar_coeff_lag;    // values:  0..3
  int ar_coeff_shift;  // values : 6..9

  // Shift value: AR coeffs range
  // 6: [-2, 2)
  // 7: [-1, 1)
  // 8: [-0.5, 0.5)
  // 9: [-0.25, 0.25)

  int cb_mult;       // 8 bits
  int cb_luma_mult;  // 8 bits
  int cb_offset;     // 9 bits
  int cr_mult;       // 8 bits

  int cr_luma_mult;  // 8 bits
  int cr_offset;     // 9 bits
  int overlap_flag;
  int clip_to_restricted_range;

  unsigned int bit_depth;  // video bit depth
  int chroma_scaling_from_luma;
  int grain_scale_shift;
  unsigned int random_seed;

  // Y 8 bit values 14*2 = 7*4
  // UV 8 bit values 10*2 = 5*4+2 (padding 2)
  int4 scaling_points[14][2];

  // Y 8 bit values 24 = 6*4
  // UV 8 bit values 25 = 6*4 + 1 (padding 3)
  int4 ar_coeffs[3][25];  // xyz = y, cb, cr
};
