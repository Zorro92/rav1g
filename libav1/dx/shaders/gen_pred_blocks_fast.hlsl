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

// Test flag -interfast: gen_pred_blocks.hlsl with the lists by motion vector class (INTER_FAST there). The
// records are the same; those of the blocks with one reference and of the averaged compound blocks go to
// the list of their class instead of the list of their type.
#define INTER_FAST 1
#include "gen_pred_blocks.hlsl"
