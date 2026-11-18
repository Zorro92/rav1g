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

#define PredListMain 0
#define PredListWarpBlock 255
#define PredListFast 279
#define PredMaxLists (PredListFast + 480)
#define PredGroupSize 64
#define PredMaxEvents 20
#define PredRankBytes (PredMaxEvents * 2)

// At the end of the group rows and ranks: lengths of the two output buffers,
// the start of the intra lists, then the list starts (one word per list).
#define PredHeaderBytes 16
#define PredIntraBase 8
