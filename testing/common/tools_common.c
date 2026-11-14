/*
 * Copyright (c) 2016, Alliance for Open Media. All rights reserved
 *
 * This source code is subject to the terms of the BSD 2 Clause License and
 * the Alliance for Open Media Patent License 1.0. If the BSD 2 Clause License
 * was not distributed with this source code in the LICENSE file, you can
 * obtain it at www.aomedia.org/license/software. If the Alliance for Open
 * Media Patent License 1.0 was not distributed with this source code in the
 * PATENTS file, you can obtain it at www.aomedia.org/license/patent.
 */

#include "tools_common.h"

#include <math.h>
#include <stdarg.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#if CONFIG_AV1_ENCODER
#include "aom/aomcx.h"
#endif

#if CONFIG_AV1_DECODER
#include "aom/aomdx.h"
#endif

#if defined(_WIN32) || defined(__OS2__)
#include <io.h>
#include <fcntl.h>

#ifdef __OS2__
#define _setmode setmode
#define _fileno fileno
#define _O_BINARY O_BINARY
#endif
#endif

#define LOG_ERROR(label)               \
  do {                                 \
    const char *l = label;             \
    va_list ap;                        \
    va_start(ap, fmt);                 \
    if (l) fprintf(stderr, "%s: ", l); \
    vfprintf(stderr, fmt, ap);         \
    fprintf(stderr, "\n");             \
    va_end(ap);                        \
  } while (0)

FILE *set_binary_mode(FILE *stream) {
  (void)stream;
#if defined(_WIN32) || defined(__OS2__)
  _setmode(_fileno(stream), _O_BINARY);
#endif
  return stream;
}

void die(const char *fmt, ...) {
  LOG_ERROR(NULL);
  usage_exit();
}

void fatal(const char *fmt, ...) {
  LOG_ERROR("Fatal");
  exit(EXIT_FAILURE);
}

void warn(const char *fmt, ...) { LOG_ERROR("Warning"); }

// What followed here in libaom's tools_common.c (die_codec, the tables of codec interfaces, reading and
// writing aom_image_t, the bit depth shifts) is gone: nothing of this tree calls it, and it needed the
// codec interface and the image functions of libav1, which the programs that compile this file no longer
// link (the decoder's parser is rav1d's parse pass, docs/phase2-rav1d.md, step 6). The stream readers
// (ivfdec.c, obudec.c) use the three functions above.
