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

#define N 4
#define ScanSize 8
#define IdctOutputShiftH 0
#define IdctOutputShiftV 4

cbuffer cb_scans_4x8 : register(b0) { int4 cb_scans[ScanSize * 3]; };

groupshared int shared_mem[8 * 64];

IDCT_GEN(N, 1, 2, 2, 2, 1, TRANSFORM_SELECT4, TRANSFORM_SELECT8);

/*
#include "idct_shader_common.h"


enum
{
    N = 4,
    ScanSize = 8,
    IdctOutputShiftH = 0,
    IdctOutputShiftV = 4,
};

typedef struct
{
    int4 table[ScanSize*3];
} Scans4x8;

typedef struct
{
    PSSLIdctData * data;
    Scans4x8 * scans;
    uint index_offset;
    uint wicount;
} PSSLIdctSRT;

thread_group_memory int shared_mem[8 * 64];

IDCT_GEN(N, 1, 2, 2, 2, 1, TRANSFORM_SELECT4, TRANSFORM_SELECT8);

*/