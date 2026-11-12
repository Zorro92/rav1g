/*
 * SPDX-License-Identifier: BSD-2-Clause
 *
 * This source code is subject to the terms of the BSD 2 Clause License and the Alliance for Open Media
 * Patent License 1.0. If the BSD 2 Clause License was not distributed with this source code in the LICENSE
 * file, you can obtain it at www.aomedia.org/license/software. If the Alliance for Open Media Patent
 * License 1.0 was not distributed with this source code in the PATENTS file, you can obtain it at
 * www.aomedia.org/license/patent.
 */

// The other direction: functions of libav1/dx/av1_core.cpp that the Rust host modules call. They have C++
// linkage, so each gets an extern "C" name here (crates/rav1g/src/host/core.rs declares them).

#include "dx/types.h"

extern "C" {

void rav1g_core_perf_gpu_mark(Av1Core *dec, int stage) { av1_perf_gpu_mark(dec, stage); }

void rav1g_core_stage_dump_residuals(Av1Core *dec, const char *stage) { av1_stage_dump_residuals(dec, stage); }

}  // extern "C"
