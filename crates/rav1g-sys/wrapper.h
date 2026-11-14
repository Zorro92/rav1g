/*
 * SPDX-License-Identifier: BSD-2-Clause
 *
 * This source code is subject to the terms of the BSD 2 Clause License and the Alliance for Open Media
 * Patent License 1.0. If the BSD 2 Clause License was not distributed with this source code in the LICENSE
 * file, you can obtain it at www.aomedia.org/license/software. If the Alliance for Open Media Patent
 * License 1.0 was not distributed with this source code in the PATENTS file, you can obtain it at
 * www.aomedia.org/license/patent.
 */

/* What bindgen reads: the decoder's C interface (aom_codec_*, the D3D12 additions of aom_decoder.h) and the
 * test instrumentation of dx/av1_perf.h. The decoder uses the types and the constants. */
#ifndef RAV1G_SYS_WRAPPER_H_
#define RAV1G_SYS_WRAPPER_H_

#include "aom/aom_codec.h"
#include "aom/aom_decoder.h"
#include "aom/aomdx.h"
#include "dx/av1_perf.h"

#ifdef __cplusplus
extern "C" {
#endif

/* av1/av1_dx_iface.c: what a decoder was created with (aom_codec_dec_cfg_t::frames_in_parse, ::threads and
 * ::frame_slots with their defaults resolved and after the budget guard's choice) and the bytes of its
 * device buffers. Nothing is written for a context without a decoder. */
void av1_codec_get_configuration(aom_codec_ctx_t *ctx, int *frames_in_parse, int *threads, int *frame_slots,
                                 size_t *device_size);

#ifdef __cplusplus
}
#endif

#endif /* RAV1G_SYS_WRAPPER_H_ */
