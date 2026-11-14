/*
 * Copyright 2020 Google LLC
 *
 */

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

#ifndef AV1_CORE_H_
#define AV1_CORE_H_

#include "av1/decoder/decoder.h"
#include "aom/aom_frame_buffer.h"
#include "av1/common/blockd.h"
#include "av1/common/onyxc_int.h"
#include "dx/av1_perf.h"

#ifdef __cplusplus
extern "C" {
#endif
int av1_query_memory_requirements(aom_codec_dec_cfg_t *cfg);
int av1_create_gpu_decoder(struct Av1Core **dec, aom_codec_dec_cfg_t *cfg);
void av1_allocate_pbi(struct Av1Core *dec, AV1Decoder **ppbi, BufferPool **pbp);
void av1_destroy_gpu_decoder(struct Av1Core *dec);
void av1_drain_gpu_decoder(struct Av1Core *dec);

// The decode call of a decoder whose parser is rav1d's (aom_codec_dec_cfg_t::parser): the frames of a
// temporal unit are parsed, recorded and submitted. Returns an aom_codec_err_t and, for an error, a text
// in *detail that lives until the next call. crates/rav1g/src/host/rav1d_host.rs.
int rav1g_rav1d_decode(struct Av1Core *dec, const uint8_t *data, size_t size, void *user_priv,
                       int operating_point, int output_all_layers, const char **detail);
// The drain of such a decoder, behind av1_drain_gpu_decoder: with more than one frame in parse
// (aom_codec_dec_cfg_t::frames_in_parse) the error of a frame that was in parse when the last decode call
// returned, one a call, AOM_CODEC_OK when there is none (always with one frame in parse).
int rav1g_rav1d_drain(struct Av1Core *dec, const char **detail);
// What the decoder was created with (aom_codec_dec_cfg_t::frames_in_parse, ::threads, ::frame_slots, each
// with its default resolved and after the budget guard's choice) and the bytes of its device buffers.
void av1_decoder_configuration(struct Av1Core *dec, int *frames_in_parse, int *threads, int *frame_slots,
                               size_t *device_size);

int av1_decode_frame_gpu(AV1Decoder *pbi);

// 1 when a sequence with this largest frame size and bit depth fits the buffers of the decoder.
int av1_sequence_fits_decoder(struct Av1Core *dec, int max_frame_width, int max_frame_height, int bit_depth,
                              int *dec_width, int *dec_height, int *dec_bitdepth);
void av1_release_fb_callback(void *priv, YV12_BUFFER_CONFIG *ybf);
int av1_reallocate_frame_buffer(void *priv, YV12_BUFFER_CONFIG *ybf, int width, int height, int upscaled_width,
                                int hbd);

void av1_setup_frame(AV1Decoder *pbi, AV1_COMMON *cm);
void av1_setup_macroblockd(AV1Decoder *pbi, AV1_COMMON *cm, ThreadData *thread_data, TileInfo *tile);
void av1_setup_sec_data(AV1Decoder *pbi, AV1_COMMON *cm, ThreadData *thread_data);
void av1_setup_ext_coef_buffer(AV1Decoder *pbi, AV1_COMMON *cm, ThreadData *thread_data);

void av1_mi_push_block(AV1Decoder *pbi, AV1_COMMON *cm, MACROBLOCKD *xd);
void av1_intra_palette(AV1Decoder *pbi, MB_MODE_INFO *mi, Av1ColorMapParam *params, int plane);

void av1_show_frame(AV1Decoder *pbi, YV12_BUFFER_CONFIG *buf, int is_visible);
void av1_decode_sef(AV1Decoder *pbi);
int get_output_frame(AV1Decoder *pbi, aom_image_t *dst);
void av1_setup_context_buffers(AV1Decoder *pbi);

// Timing instrumentation. av1_perf_ticks() returns 0 while timing is off, and
// av1_perf_cpu_add() ignores a zero start, so call sites need no checks.
int64_t av1_perf_ticks(void);
void av1_perf_cpu_add(AV1Decoder *pbi, int id, int64_t start_ticks);
#ifdef __cplusplus
}
#endif

#endif