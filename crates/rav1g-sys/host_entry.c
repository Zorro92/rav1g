/*
 * SPDX-License-Identifier: BSD-2-Clause
 *
 * This source code is subject to the terms of the BSD 2 Clause License and the Alliance for Open Media
 * Patent License 1.0. If the BSD 2 Clause License was not distributed with this source code in the LICENSE
 * file, you can obtain it at www.aomedia.org/license/software. If the Alliance for Open Media Patent
 * License 1.0 was not distributed with this source code in the PATENTS file, you can obtain it at
 * www.aomedia.org/license/patent.
 */

/* The functions of the host (dx/av1_core.h) that can end with the decoder's error. aom_internal_error leaves
 * the decode call with a longjmp when the caller has set one up, and a longjmp must not pass a Rust function.
 * So each of these is C: it calls the parts of the Rust function (crates/rav1g/src/host/frame.rs,
 * completion.rs) one after
 * the other and raises the error between them. When the error returns (no jump set up), the function goes
 * on with its next part, as the C++ went on behind aom_internal_error. */

#include "dx/av1_core.h"

int rav1g_setup_frame_begin(AV1Decoder *pbi, AV1_COMMON *cm, int64_t *perf_start);
int rav1g_setup_frame_coefs(AV1Decoder *pbi, AV1_COMMON *cm);
int rav1g_setup_frame_end(AV1Decoder *pbi, AV1_COMMON *cm, int64_t perf_start);
int rav1g_decode_frame_record(AV1Decoder *pbi, av1_decoded_frame_buffer_t *fb, int64_t *perf_lap_start);
void rav1g_decode_frame_submit(AV1Decoder *pbi, int image_state, const av1_decoded_frame_buffer_t *fb,
                               int64_t perf_lap_start);
int rav1g_decode_sef_begin(AV1Decoder *pbi, av1_decoded_frame_buffer_t *fb);
void rav1g_decode_sef_submit(AV1Decoder *pbi, int image_state, const av1_decoded_frame_buffer_t *fb);

void av1_setup_frame(AV1Decoder *pbi, AV1_COMMON *cm) {
  int64_t perf_start = 0;
  if (rav1g_setup_frame_begin(pbi, cm, &perf_start))
    aom_internal_error(&cm->error, AOM_CODEC_MEM_ERROR,
                       "No palette buffer for a frame that allows screen content tools");
  if (rav1g_setup_frame_coefs(pbi, cm))
    aom_internal_error(&cm->error, AOM_CODEC_MEM_ERROR,
                       "No room for the coefficients of a frame with very small tiles");
  if (rav1g_setup_frame_end(pbi, cm, perf_start))
    aom_internal_error(&cm->error, AOM_CODEC_MEM_ERROR, "Failed to start GPU completion thread.");
}

void av1_decode_sef(AV1Decoder *pbi) {
  av1_decoded_frame_buffer_t fb = { 0 };
  /* -1: nothing to output; else the state of the output image, as below. */
  const int image_state = rav1g_decode_sef_begin(pbi, &fb);
  if (image_state < 0) return;
  if (image_state == 2)
    aom_internal_error(&pbi->common.error, AOM_CODEC_MEM_ERROR, "Failed to allocate output frame.");
  rav1g_decode_sef_submit(pbi, image_state, &fb);
}

int av1_decode_frame_gpu(AV1Decoder *pbi) {
  av1_decoded_frame_buffer_t fb = { 0 };
  int64_t perf_lap_start = 0;
  /* 0: the frame is not output; 1: fb is its output image; 2: the application gave none. */
  const int image_state = rav1g_decode_frame_record(pbi, &fb, &perf_lap_start);
  if (image_state == 2)
    aom_internal_error(&pbi->common.error, AOM_CODEC_MEM_ERROR, "Failed to allocate output frame.");
  rav1g_decode_frame_submit(pbi, image_state, &fb, perf_lap_start);
  return 0;
}
