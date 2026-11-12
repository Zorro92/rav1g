/*
 * The reference side of superres_check: libaom's normative upscale of this tree
 * (av1_upscale_normative_rows in av1/common/resize.c), one plane at a time.
 */
#ifndef SUPERRES_CHECK_REF_GLUE_H_
#define SUPERRES_CHECK_REF_GLUE_H_

#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

typedef struct RefUpscale {
  int upscaled_width;   /* luma, the frame's width after the upscale */
  int denominator;      /* superres_denom, 9..16 */
  int plane;            /* 0 luma, 1 or 2 chroma (4:2:0) */
  int rows;             /* rows of the plane */
  int bit_depth;        /* 8, 10 or 12 */
  int use_highbitdepth; /* samples are 16 bits wide */
  int sb_size_log2;     /* 6 or 7 */
  int tile_cols;
  int tile_col_start_sb[65]; /* tile_cols + 1 entries, in superblocks */
} RefUpscale;

/* The coded (downscaled) luma width for an upscaled width and a denominator, as the frame header gives it. */
int ref_downscaled_width(int upscaled_width, int denominator);

/* Upscales |rows| rows of one plane. src and dst point at the first sample of the first row; strides in
 * samples. src must have 8 samples to the left of each row and its samples up to the 8-aligned frame width
 * (libaom reads the row up to the last mode info column). */
void ref_upscale_plane(const RefUpscale *u, const void *src, int src_stride, void *dst, int dst_stride);

#ifdef __cplusplus
}
#endif

#endif /* SUPERRES_CHECK_REF_GLUE_H_ */
