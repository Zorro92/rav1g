/*
 * SPDX-License-Identifier: BSD-2-Clause
 *
 * This source code is subject to the terms of the BSD 2 Clause License and the Alliance for Open Media
 * Patent License 1.0. If the BSD 2 Clause License was not distributed with this source code in the LICENSE
 * file, you can obtain it at www.aomedia.org/license/software. If the Alliance for Open Media Patent
 * License 1.0 was not distributed with this source code in the PATENTS file, you can obtain it at
 * www.aomedia.org/license/patent.
 */

// The functions of libav1/dx/prediction.cpp for the C++ that remains in the cargo build: calls of the Rust
// that has what they do (crates/rav1g/src/host/prediction.rs). av1_mi_push_block, which the entropy decoder
// calls for every block, and av1_set_intra_opt and av1_set_intra_edge_fix are Rust functions of those names
// (host/blocks.rs, host/prediction.rs).

#include "dx/types.h"
#include "dx/av1_core.h"

extern "C" {
void rav1g_prediction_gen_blocks(AV1Decoder *pbi, Av1Core *dec);
void rav1g_prediction_run_all(Av1Core *dec, AV1_COMMON *cm, TileInfo *tile);
void rav1g_inter_ext_borders(Av1Core *dec, AV1_COMMON *cm);
}

void av1_prediction_gen_blocks(AV1Decoder *pbi, Av1Core *dec) { rav1g_prediction_gen_blocks(pbi, dec); }

void av1_prediction_run_all(Av1Core *dec, AV1_COMMON *cm, TileInfo *tile) { rav1g_prediction_run_all(dec, cm, tile); }

void av1_inter_ext_borders(Av1Core *dec, AV1_COMMON *cm) { rav1g_inter_ext_borders(dec, cm); }
