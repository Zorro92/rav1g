/*
 * The reference side of warp_check: libaom's C code from this tree
 * (av1/common/reconinter.c, warped_motion.c, convolve.c, scale.c and
 * aom_dsp/blend_a64_mask.c), called the way libaom's decoder calls it for one
 * block. ref_glue.c is C, warp_check.cpp is C++.
 */
#ifndef WARP_CHECK_REF_GLUE_H_
#define WARP_CHECK_REF_GLUE_H_

#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

enum { REF_SIDE_MV = 0, REF_SIDE_GLOBAL_WARP = 1, REF_SIDE_LOCAL_WARP = 2 };
enum { REF_COMP_AVERAGE = 0, REF_COMP_DISTANCE = 1, REF_COMP_WEDGE = 2, REF_COMP_DIFFWTD = 3 };

/* One reference of a block. */
typedef struct RefSide {
  const void *origin; /* sample (0, 0) of the reference plane: uint8_t, or uint16_t above 8 bits. The plane
                         is readable far beyond its edges, where the edge samples are repeated. */
  int stride;         /* in samples */
  int width, height;  /* of the plane */
  int kind;           /* REF_SIDE_* */
  int mat[6];         /* the warp model (wmmat), for the two warp kinds */
  int mv_row, mv_col; /* 1/8 luma sample, for REF_SIDE_MV */
} RefSide;

/* One plane of one block. */
typedef struct RefBlock {
  int bit_depth;      /* 8 or 10 */
  int plane;          /* 0, 1, 2 */
  int luma_w, luma_h; /* block size in luma samples */
  int mi_row, mi_col; /* block position in units of 4 luma samples */
  int refs;           /* 1 or 2 */
  RefSide side[2];
  int filter_x, filter_y;     /* 0 regular, 1 smooth, 2 sharp, 3 bilinear */
  int comp_type;              /* REF_COMP_*, for two references */
  int fwd_offset, bck_offset; /* REF_COMP_DISTANCE */
  int wedge_index, wedge_sign;
  int mask_type;     /* REF_COMP_DIFFWTD */
  uint8_t *seg_mask; /* REF_COMP_DIFFWTD: luma_w * luma_h bytes, written by the luma call of the block and read by
                        its chroma calls */
  void *dst;         /* where the block goes in the plane: uint8_t or uint16_t samples */
  int dst_stride;    /* in samples */
} RefBlock;

void ref_init(void);
void ref_predict(const RefBlock *block);

/* Shear parameters of a warp model (get_shear_params); returns 0 if libaom refuses the model. */
int ref_shear(const int *mat, int16_t *alpha_beta_gamma_delta);
/* Local warp model from np sample pairs (find_projection); returns 0 if libaom marks the model invalid. */
int ref_local_warp(int np, int *pts, int *pts_inref, int luma_w, int luma_h, int mv_row, int mv_col, int mi_row,
                   int mi_col, int *mat, int16_t *alpha_beta_gamma_delta);
/* The motion vector of a GLOBALMV block for a model of the given type (gm_get_motion_vector). */
void ref_global_mv(const int *mat, int wmtype, int allow_hp, int luma_w, int luma_h, int mi_row, int mi_col,
                   int *mv_row, int *mv_col);
/* scaled_x() of scale.c for a position in 1/16 sample and a scale in 1/16384. */
int ref_scale_value(int val, int scale);
/* The scale factors of a reference of ref_w x ref_h for a frame of w x h (av1_setup_scale_factors_for_frame), as
 * av1_setup_frame() of av1_core.cpp hands them to the shaders: x scale, x step, y scale, y step. */
void ref_scale_factors(int ref_w, int ref_h, int w, int h, int *scale);

/* Tables the decoder uploads: the warp filter (193 x 8), the interpolation kernels in the order of
 * prediction.cpp, and the wedge masks laid out like inter_mask_lut of av1_compute.cpp. */
const int16_t *ref_warp_filter(void);
void ref_kernels(int kernels[8 * 16][8]);
size_t ref_wedge_lut(uint8_t *lut, size_t capacity, int offsets[22][2]);
int ref_block_size(int luma_w, int luma_h); /* BLOCK_SIZE, or -1 */
int ref_wedge_bits(int block_size);
void ref_distance_weights(int index, int order, int *fwd_offset, int *bck_offset);

#ifdef __cplusplus
}
#endif

#endif /* WARP_CHECK_REF_GLUE_H_ */
