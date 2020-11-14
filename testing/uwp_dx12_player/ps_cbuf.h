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

#ifndef _PS_CBUF_H_
#define _PS_CBUF_H_
#include "hlsl_cb.h"

// YUV2RGB_MATRIX
#define MATRIX_BT709 0
#define MATRIX_BT2020 1

// enum TRANSFER_FUNC
#define TFUNC_SRGB 0
#define TFUNC_BT709 1
#define TFUNC_2084 2  // SMPTE ST 2084
#define TFUNC_HGL 3   // ARIB STD-B67

// enum PRIMERY_COLORS
#define PRIM_709 0
#define PRIM_2020 1

// enum YUV_RANGE
#define RANGE_LIMITED 0
#define RANGE_FULL 1

CONSTANT_BUFFER_DEF(CbPixShData, b0) {
  float scale_factor;
  int src_matrix;
  int src_range;
  int src_func;
  int src_prim;
  int targ_func;
  int targ_prim;
  float lum_max;
  float lum_min;
  int isMonochrome;
  int hdr_10_10_10_2;
  int disp_w;
  int disp_h;
  int frame_w;
  int frame_h;
  int ptex_w;
  int ptex_h;
};

#endif  //_PS_CBUF_H_
