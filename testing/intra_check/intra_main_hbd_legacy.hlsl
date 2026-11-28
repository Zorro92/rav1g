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

// The original kernel (the INTRA_MAIN_LEGACY parts of libav1/dx/shaders/intra_main_hbd.hlsl), the reference of
// intra_check. Not a shader of the decoder any more: its runtime switch, -intraod, is retired, and the
// shader build does not see this file.
#define INTRA_MAIN_LEGACY
#include "intra_main_hbd.hlsl"
