/*
 * SPDX-License-Identifier: BSD-2-Clause
 *
 * This source code is subject to the terms of the BSD 2 Clause License and the Alliance for Open Media
 * Patent License 1.0. If the BSD 2 Clause License was not distributed with this source code in the LICENSE
 * file, you can obtain it at www.aomedia.org/license/software. If the Alliance for Open Media Patent
 * License 1.0 was not distributed with this source code in the PATENTS file, you can obtain it at
 * www.aomedia.org/license/patent.
 */

// The block paths of the display pass (display_pass.hlsl, quad_block: exactly 1:1 and 3:2) for SDR (BT.709
// or BT.2020 primaries, any transfer but PQ and HLG) frames: their own pipeline, which the host takes for a
// frame with bit 8 or 9 of the flags, so that the 2:1 path and the area loop (display_pass_sdr.hlsl) have
// neither their registers nor their code.
#define DISPLAY_MODE 1
#define DISPLAY_PATHS 2
#include "display_pass.hlsl"
