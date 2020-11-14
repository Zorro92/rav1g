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

#include "idct_shader_common.h"

#define N 64
#define ScanSize 256
#define IdctOutputShiftH 2
#define IdctOutputShiftV 4

cbuffer cb_scans_64x64 : register(b0) { int4 cb_scans[ScanSize * 3]; };

groupshared int shared_mem[64 * 64];

IDCT_GEN(N, 1, 1, 1, 4, 0, TRANSFORM_64_HALF, TRANSFORM_64);

/*
#include "idct_shader_common.h"


enum
{
    N = 64,
    ScanSize = 256,
    IdctOutputShiftH = 2,
    IdctOutputShiftV = 4,
};

typedef struct
{
    int4 table[ScanSize*3];
} Scans;

typedef struct
{
    PSSLIdctData * data;
    Scans * scans;
    uint index_offset;
    uint wicount;
} PSSLIdctSRT;

thread_group_memory int shared_mem[64 * 64];

IDCT_GEN(N, 1, 1, 1, 4, 0, TRANSFORM_64_HALF, TRANSFORM_64);


*/
