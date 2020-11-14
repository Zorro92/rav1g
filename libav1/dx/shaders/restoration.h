/*
 * Copyright 2020 Google LLC
 *
 */

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

#ifndef AOM_AV1_COMMON_RESTORATION_H_
#define AOM_AV1_COMMON_RESTORATION_H_

#ifdef __cplusplus
extern "C" {
#endif

// Border for Loop restoration buffer
#define AOM_RESTORATION_FRAME_BORDER 32
#define CLIP(x, lo, hi) ((x) < (lo) ? (lo) : (x) > (hi) ? (hi) : (x))
#define RINT(x) ((x) < 0 ? (int)((x)-0.5) : (int)((x) + 0.5))

#define RESTORATION_PROC_UNIT_SIZE 64

// Filter tile grid offset upwards compared to the superblock grid
#define RESTORATION_UNIT_OFFSET 8

#define SGRPROJ_BORDER_VERT 3  // Vertical border used for Sgr
#define SGRPROJ_BORDER_HORZ 3  // Horizontal border used for Sgr

#define WIENER_BORDER_VERT 2  // Vertical border used for Wiener
#define WIENER_HALFWIN 3
#define WIENER_BORDER_HORZ (WIENER_HALFWIN)  // Horizontal border for Wiener

// RESTORATION_BORDER_VERT determines line buffer requirement for LR.
// Should be set at the max of SGRPROJ_BORDER_VERT and WIENER_BORDER_VERT.
// Note the line buffer needed is twice the value of this macro.
#if SGRPROJ_BORDER_VERT >= WIENER_BORDER_VERT
#define RESTORATION_BORDER_VERT (SGRPROJ_BORDER_VERT)
#else
#define RESTORATION_BORDER_VERT (WIENER_BORDER_VERT)
#endif  // SGRPROJ_BORDER_VERT >= WIENER_BORDER_VERT

#if SGRPROJ_BORDER_HORZ >= WIENER_BORDER_HORZ
#define RESTORATION_BORDER_HORZ (SGRPROJ_BORDER_HORZ)
#else
#define RESTORATION_BORDER_HORZ (WIENER_BORDER_HORZ)
#endif  // SGRPROJ_BORDER_VERT >= WIENER_BORDER_VERT

// How many border pixels do we need for each processing unit?
#define RESTORATION_BORDER 3

// How many rows of deblocked pixels do we save above/below each processing
// stripe?
#define RESTORATION_CTX_VERT 2

// Additional pixels to the left and right in above/below buffers
// It is RESTORATION_BORDER_HORZ rounded up to get nicer buffer alignment
#define RESTORATION_EXTRA_HORZ 4

// Pad up to 20 more (may be much less is needed)
#define RESTORATION_PADDING 20
#define RESTORATION_PROC_UNIT_PELS                                                    \
  ((RESTORATION_PROC_UNIT_SIZE + RESTORATION_BORDER_HORZ * 2 + RESTORATION_PADDING) * \
   (RESTORATION_PROC_UNIT_SIZE + RESTORATION_BORDER_VERT * 2 + RESTORATION_PADDING))

#define RESTORATION_UNITSIZE_MAX 256
#define RESTORATION_UNITPELS_HORZ_MAX (RESTORATION_UNITSIZE_MAX * 3 / 2 + 2 * RESTORATION_BORDER_HORZ + 16)
#define RESTORATION_UNITPELS_VERT_MAX \
  ((RESTORATION_UNITSIZE_MAX * 3 / 2 + 2 * RESTORATION_BORDER_VERT + RESTORATION_UNIT_OFFSET))
#define RESTORATION_UNITPELS_MAX (RESTORATION_UNITPELS_HORZ_MAX * RESTORATION_UNITPELS_VERT_MAX)

// Two 32-bit buffers needed for the restored versions from two filters
// TODO(debargha, rupert): Refactor to not need the large tilesize to be stored
// on the decoder side.
#define SGRPROJ_TMPBUF_SIZE (RESTORATION_UNITPELS_MAX * 2 * sizeof(int32_t))

#define SGRPROJ_EXTBUF_SIZE (0)
#define SGRPROJ_PARAMS_BITS 4
#define SGRPROJ_PARAMS (1 << SGRPROJ_PARAMS_BITS)

// Precision bits for projection
#define SGRPROJ_PRJ_BITS 7
// Restoration precision bits generated higher than source before projection
#define SGRPROJ_RST_BITS 4
// Internal precision bits for core selfguided_restoration
#define SGRPROJ_SGR_BITS 8
#define SGRPROJ_SGR (1 << SGRPROJ_SGR_BITS)

#define SGRPROJ_PRJ_MIN0 (-(1 << SGRPROJ_PRJ_BITS) * 3 / 4)
#define SGRPROJ_PRJ_MAX0 (SGRPROJ_PRJ_MIN0 + (1 << SGRPROJ_PRJ_BITS) - 1)
#define SGRPROJ_PRJ_MIN1 (-(1 << SGRPROJ_PRJ_BITS) / 4)
#define SGRPROJ_PRJ_MAX1 (SGRPROJ_PRJ_MIN1 + (1 << SGRPROJ_PRJ_BITS) - 1)

#define SGRPROJ_PRJ_SUBEXP_K 4

#define SGRPROJ_BITS (SGRPROJ_PRJ_BITS * 2 + SGRPROJ_PARAMS_BITS)

#define MAX_RADIUS 2  // Only 1, 2, 3 allowed
#define MAX_NELEM ((2 * MAX_RADIUS + 1) * (2 * MAX_RADIUS + 1))
#define SGRPROJ_MTABLE_BITS 20
#define SGRPROJ_RECIP_BITS 12

#define WIENER_HALFWIN1 (WIENER_HALFWIN + 1)
#define WIENER_WIN (2 * WIENER_HALFWIN + 1)
#define WIENER_WIN2 ((WIENER_WIN) * (WIENER_WIN))
#define WIENER_TMPBUF_SIZE (0)
#define WIENER_EXTBUF_SIZE (0)

// If WIENER_WIN_CHROMA == WIENER_WIN - 2, that implies 5x5 filters are used for
// chroma. To use 7x7 for chroma set WIENER_WIN_CHROMA to WIENER_WIN.
#define WIENER_WIN_CHROMA (WIENER_WIN - 2)
#define WIENER_WIN2_CHROMA ((WIENER_WIN_CHROMA) * (WIENER_WIN_CHROMA))

#define WIENER_FILT_PREC_BITS 7
#define WIENER_FILT_STEP (1 << WIENER_FILT_PREC_BITS)

// Central values for the taps
#define WIENER_FILT_TAP0_MIDV (3)
#define WIENER_FILT_TAP1_MIDV (-7)
#define WIENER_FILT_TAP2_MIDV (15)
#define WIENER_FILT_TAP3_MIDV \
  (WIENER_FILT_STEP - 2 * (WIENER_FILT_TAP0_MIDV + WIENER_FILT_TAP1_MIDV + WIENER_FILT_TAP2_MIDV))

#define WIENER_FILT_TAP0_BITS 4
#define WIENER_FILT_TAP1_BITS 5
#define WIENER_FILT_TAP2_BITS 6

#define WIENER_FILT_BITS ((WIENER_FILT_TAP0_BITS + WIENER_FILT_TAP1_BITS + WIENER_FILT_TAP2_BITS) * 2)

#define WIENER_FILT_TAP0_MINV (WIENER_FILT_TAP0_MIDV - (1 << WIENER_FILT_TAP0_BITS) / 2)
#define WIENER_FILT_TAP1_MINV (WIENER_FILT_TAP1_MIDV - (1 << WIENER_FILT_TAP1_BITS) / 2)
#define WIENER_FILT_TAP2_MINV (WIENER_FILT_TAP2_MIDV - (1 << WIENER_FILT_TAP2_BITS) / 2)

#define WIENER_FILT_TAP0_MAXV (WIENER_FILT_TAP0_MIDV - 1 + (1 << WIENER_FILT_TAP0_BITS) / 2)
#define WIENER_FILT_TAP1_MAXV (WIENER_FILT_TAP1_MIDV - 1 + (1 << WIENER_FILT_TAP1_BITS) / 2)
#define WIENER_FILT_TAP2_MAXV (WIENER_FILT_TAP2_MIDV - 1 + (1 << WIENER_FILT_TAP2_BITS) / 2)

#define WIENER_FILT_TAP0_SUBEXP_K 1
#define WIENER_FILT_TAP1_SUBEXP_K 2
#define WIENER_FILT_TAP2_SUBEXP_K 3

// Max of SGRPROJ_TMPBUF_SIZE, DOMAINTXFMRF_TMPBUF_SIZE, WIENER_TMPBUF_SIZE
#define RESTORATION_TMPBUF_SIZE (SGRPROJ_TMPBUF_SIZE)

// Max of SGRPROJ_EXTBUF_SIZE, WIENER_EXTBUF_SIZE
#define RESTORATION_EXTBUF_SIZE (WIENER_EXTBUF_SIZE)

#if WIENER_FILT_PREC_BITS != 7
#error "Wiener filter currently only works if WIENER_FILT_PREC_BITS == 7"
#endif

#define LR_TILE_ROW 0
#define LR_TILE_COL 0
#define LR_TILE_COLS 1
#ifdef __cplusplus
}  // extern "C"
#endif

#endif  // AOM_AV1_COMMON_RESTORATION_H_
