/*
 * SPDX-License-Identifier: BSD-2-Clause
 *
 * This source code is subject to the terms of the BSD 2 Clause License and the Alliance for Open Media
 * Patent License 1.0. If the BSD 2 Clause License was not distributed with this source code in the LICENSE
 * file, you can obtain it at www.aomedia.org/license/software. If the Alliance for Open Media Patent
 * License 1.0 was not distributed with this source code in the PATENTS file, you can obtain it at
 * www.aomedia.org/license/patent.
 */

/* The entry points of the C interface that are macros, as functions Rust can call (wrapper.h). */

#include "config/aom_config.h"

#include "wrapper.h"

aom_codec_err_t rav1g_codec_dec_init(aom_codec_ctx_t *ctx, const aom_codec_dec_cfg_t *cfg, aom_codec_flags_t flags) {
  return aom_codec_dec_init(ctx, aom_codec_av1_dx(), cfg, flags);
}

aom_codec_err_t rav1g_codec_peek_stream_info(const uint8_t *data, size_t data_size, aom_codec_stream_info_t *si) {
  return aom_codec_peek_stream_info(aom_codec_av1_dx(), data, data_size, si);
}

aom_codec_err_t rav1g_codec_set_output_all_layers(aom_codec_ctx_t *ctx, int enable) {
  return aom_codec_control(ctx, AV1D_SET_OUTPUT_ALL_LAYERS, enable);
}

aom_codec_err_t rav1g_codec_set_operating_point(aom_codec_ctx_t *ctx, int operating_point) {
  return aom_codec_control(ctx, AV1D_SET_OPERATING_POINT, operating_point);
}

int rav1g_config_lowbitdepth(void) { return CONFIG_LOWBITDEPTH; }
