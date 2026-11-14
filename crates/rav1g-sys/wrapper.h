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
 * test instrumentation of dx/av1_perf.h, and the entry points of helpers.c. */
#ifndef RAV1G_SYS_WRAPPER_H_
#define RAV1G_SYS_WRAPPER_H_

#include "aom/aom_codec.h"
#include "aom/aom_decoder.h"
#include "aom/aomdx.h"
#include "dx/av1_perf.h"

#ifdef __cplusplus
extern "C" {
#endif

/* aom_codec_dec_init(ctx, aom_codec_av1_dx(), cfg, flags): the macro passes the ABI version of the headers. */
aom_codec_err_t rav1g_codec_dec_init(aom_codec_ctx_t *ctx, const aom_codec_dec_cfg_t *cfg, aom_codec_flags_t flags);

/* aom_codec_peek_stream_info(aom_codec_av1_dx(), ...). */
aom_codec_err_t rav1g_codec_peek_stream_info(const uint8_t *data, size_t data_size, aom_codec_stream_info_t *si);

/* aom_codec_control(ctx, AV1D_SET_OUTPUT_ALL_LAYERS, enable) and (ctx, AV1D_SET_OPERATING_POINT, point):
 * aom_codec_control is a macro over a function with a variable argument list. */
aom_codec_err_t rav1g_codec_set_output_all_layers(aom_codec_ctx_t *ctx, int enable);
aom_codec_err_t rav1g_codec_set_operating_point(aom_codec_ctx_t *ctx, int operating_point);
/* av1/av1_dx_iface.c: what a decoder was created with (aom_codec_dec_cfg_t::frames_in_parse, ::threads and
 * ::frame_slots with their defaults resolved and after the budget guard's choice) and the bytes of its
 * device buffers. Nothing is written for a context without a decoder. */
void av1_codec_get_configuration(aom_codec_ctx_t *ctx, int *frames_in_parse, int *threads, int *frame_slots,
                                 size_t *device_size);

/* CONFIG_LOWBITDEPTH of the build, the value the player gives aom_codec_dec_cfg_t::allow_lowbitdepth. */
int rav1g_config_lowbitdepth(void);

#ifdef __cplusplus
}
#endif

#endif /* RAV1G_SYS_WRAPPER_H_ */
