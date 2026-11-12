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

void rav1g_core_perf_add(Av1Core *dec, int id, int64_t start_ticks) { av1_perf_add(dec, id, start_ticks); }

void rav1g_core_stage_dump_residuals(Av1Core *dec, const char *stage) { av1_stage_dump_residuals(dec, stage); }

void rav1g_core_stage_dump_fb(Av1Core *dec, const char *stage, const HwFrameBuffer *fb, const HwFrameBuffer *dims) {
  av1_stage_dump_fb(dec, stage, fb, dims);
}

void rav1g_core_stage_dump_output(Av1Core *dec, const char *stage, const HwOutputImage *img, size_t size) {
  av1_stage_dump_output(dec, stage, img, size);
}

void rav1g_core_stage_dump_data(Av1Core *dec, const char *stage, const char *layout,
                                const av1_stage_dump_region *regions, int count) {
  av1_stage_dump_data(dec, stage, layout, regions, count);
}

}  // extern "C"
