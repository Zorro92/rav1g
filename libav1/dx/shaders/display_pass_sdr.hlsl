/*
 * SPDX-License-Identifier: BSD-2-Clause
 *
 * This source code is subject to the terms of the BSD 2 Clause License and the Alliance for Open Media
 * Patent License 1.0. If the BSD 2 Clause License was not distributed with this source code in the LICENSE
 * file, you can obtain it at www.aomedia.org/license/software. If the Alliance for Open Media Patent
 * License 1.0 was not distributed with this source code in the PATENTS file, you can obtain it at
 * www.aomedia.org/license/patent.
 */

// The display pass (display_pass.hlsl) for SDR (BT.709 or BT.2020 primaries, any transfer but PQ and HLG) frames: its own pipeline, with the registers of
// that mode's path only.
#define DISPLAY_MODE 1
#include "display_pass.hlsl"
