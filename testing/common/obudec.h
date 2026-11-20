/*
 * Copyright (c) 2017, Alliance for Open Media. All rights reserved
 *
 * This source code is subject to the terms of the BSD 2 Clause License and
 * the Alliance for Open Media Patent License 1.0. If the BSD 2 Clause License
 * was not distributed with this source code in the LICENSE file, you can
 * obtain it at www.aomedia.org/license/software. If the Alliance for Open
 * Media Patent License 1.0 was not distributed with this source code in the
 * PATENTS file, you can obtain it at www.aomedia.org/license/patent.
 */
#ifndef AOM_COMMON_OBUDEC_H_
#define AOM_COMMON_OBUDEC_H_

#include "common/tools_common.h"

#ifdef __cplusplus
extern "C" {
#endif

struct ObuDecInputContext {
  struct AvxInputContext *avx_ctx;
  uint8_t *buffer;
  size_t buffer_capacity;
  size_t bytes_buffered;
  int is_annexb;
};

// Probes a low-overhead stream for an initial delimiter or sequence header.
int file_is_obu(struct ObuDecInputContext *obu_ctx);

// Reads one Temporal Unit from the input file. Returns 0 when a TU is
// successfully read, 1 at EOF, -1 for invalid framing, and -2 for I/O or
// allocation failure. Annex B units are converted to low-overhead OBUs.
// The caller owns 'buffer', with capacity in 'buffer_size'.
int obudec_read_temporal_unit(struct ObuDecInputContext *obu_ctx, uint8_t **buffer, size_t *bytes_read,
                              size_t *buffer_size);

void obudec_free(struct ObuDecInputContext *obu_ctx);

#ifdef __cplusplus
} /* extern "C" */
#endif

#endif  // AOM_COMMON_OBUDEC_H_
