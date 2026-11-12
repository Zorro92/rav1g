/*
 * The reference side of warp_check: one block predicted by libaom's C code of
 * this tree. See ref_glue.h.
 *
 * libaom's decoder builds an inter block with dec_build_inter_predictors()
 * and av1_make_masked_inter_predictor(); the fork removed both (the GPU does
 * that work), so ref_predict() below repeats their steps around the functions
 * that are still in the tree: av1_make_inter_predictor() with av1_allow_warp()
 * and av1_warp_plane(), the convolve functions, the difference weighted mask
 * and the mask blend.
 */
#include <string.h>

#include "config/aom_config.h"
#include "config/aom_dsp_rtcd.h"
#include "config/av1_rtcd.h"

#include "aom_dsp/blend.h"
#include "av1/common/blockd.h"
#include "av1/common/common_data.h"
#include "av1/common/convolve.h"
#include "av1/common/filter.h"
#include "av1/common/mv.h"
#include "av1/common/reconinter.h"
#include "av1/common/scale.h"
#include "av1/common/warped_motion.h"

#include "ref_glue.h"

/* Referenced by reconinter.c, not reached from ref_predict(). */
void aom_internal_error(struct aom_internal_error_info *info, aom_codec_err_t error, const char *fmt, ...) {
  (void)info;
  (void)error;
  (void)fmt;
  abort();
}
void av1_predict_intra_block(const AV1_COMMON *cm, const MACROBLOCKD *xd, int wpx, int hpx, TX_SIZE tx_size,
                             PREDICTION_MODE mode, int angle_delta, int use_palette,
                             FILTER_INTRA_MODE filter_intra_mode, const uint8_t *ref, int ref_stride, uint8_t *dst,
                             int dst_stride, int col_off, int row_off, int plane) {
  (void)cm, (void)xd, (void)wpx, (void)hpx, (void)tx_size, (void)mode, (void)angle_delta, (void)use_palette;
  (void)filter_intra_mode, (void)ref, (void)ref_stride, (void)dst, (void)dst_stride, (void)col_off, (void)row_off;
  (void)plane;
  abort();
}

static struct scale_factors g_unscaled;

void ref_init(void) {
  av1_init_wedge_masks();
  av1_setup_scale_factors_for_frame(&g_unscaled, 64, 64, 64, 64);
}

int ref_block_size(int luma_w, int luma_h) {
  for (int bsize = 0; bsize < BLOCK_SIZES_ALL; ++bsize)
    if (block_size_wide[bsize] == luma_w && block_size_high[bsize] == luma_h) return bsize;
  return -1;
}

int ref_wedge_bits(int block_size) { return wedge_params_lookup[block_size].bits; }

void ref_distance_weights(int index, int order, int *fwd_offset, int *bck_offset) {
  *fwd_offset = quant_dist_lookup_table[0][index][order];
  *bck_offset = quant_dist_lookup_table[0][index][1 - order];
}

static void set_model(WarpedMotionParams *wm, const int *mat) {
  *wm = default_warp_params;
  for (int i = 0; i < 6; ++i) wm->wmmat[i] = mat[i];
  wm->wmtype = AFFINE;
}

int ref_shear(const int *mat, int16_t *abgd) {
  WarpedMotionParams wm;
  set_model(&wm, mat);
  const int ok = get_shear_params(&wm);
  abgd[0] = wm.alpha;
  abgd[1] = wm.beta;
  abgd[2] = wm.gamma;
  abgd[3] = wm.delta;
  return ok;
}

int ref_local_warp(int np, int *pts, int *pts_inref, int luma_w, int luma_h, int mv_row, int mv_col, int mi_row,
                   int mi_col, int *mat, int16_t *abgd) {
  WarpedMotionParams wm = default_warp_params;
  /* As read_inter_block_mode_info() of decodemv.c does for a WARPED_CAUSAL block. */
  if (find_projection(np, pts, pts_inref, (BLOCK_SIZE)ref_block_size(luma_w, luma_h), mv_row, mv_col, &wm, mi_row,
                      mi_col))
    return 0;
  for (int i = 0; i < 6; ++i) mat[i] = wm.wmmat[i];
  abgd[0] = wm.alpha;
  abgd[1] = wm.beta;
  abgd[2] = wm.gamma;
  abgd[3] = wm.delta;
  return 1;
}

void ref_global_mv(const int *mat, int wmtype, int allow_hp, int luma_w, int luma_h, int mi_row, int mi_col,
                   int *mv_row, int *mv_col) {
  WarpedMotionParams wm;
  set_model(&wm, mat);
  wm.wmtype = (TransformationType)wmtype;
  const int_mv mv = gm_get_motion_vector(&wm, allow_hp, (BLOCK_SIZE)ref_block_size(luma_w, luma_h), mi_col, mi_row, 0);
  *mv_row = mv.as_mv.row;
  *mv_col = mv.as_mv.col;
}

int ref_scale_value(int val, int scale) {
  /* A reference 'scale' samples wide for a frame 16384 samples wide has exactly this scale. */
  struct scale_factors sf;
  av1_setup_scale_factors_for_frame(&sf, scale, scale, 1 << REF_SCALE_SHIFT, 1 << REF_SCALE_SHIFT);
  if (sf.x_scale_fp != scale) abort();
  return sf.scale_value_x(val, &sf);
}

void ref_scale_factors(int ref_w, int ref_h, int w, int h, int *scale) {
  struct scale_factors sf;
  av1_setup_scale_factors_for_frame(&sf, ref_w, ref_h, w, h);
  if (!av1_is_valid_scale(&sf)) abort(); /* the check only makes sizes a reference can have */
  scale[0] = sf.x_scale_fp;
  scale[1] = sf.x_step_q4;
  scale[2] = sf.y_scale_fp;
  scale[3] = sf.y_step_q4;
}

const int16_t *ref_warp_filter(void) { return &warped_filter[0][0]; }

void ref_kernels(int kernels[8 * 16][8]) {
  /* As av1_prediction_run_all() of prediction.cpp fills PSSLInterData::kernels. */
  const int16_t *ker_arr[] = {
      (const int16_t *)av1_sub_pel_filters_4,       (const int16_t *)av1_sub_pel_filters_4smooth,
      (const int16_t *)av1_bilinear_filters,        (const int16_t *)av1_sub_pel_filters_8,
      (const int16_t *)av1_sub_pel_filters_8smooth, (const int16_t *)av1_sub_pel_filters_8sharp};
  memset(kernels, 0, sizeof(int) * 8 * 16 * 8);
  for (int i = 0; i < 6; ++i)
    for (int j = 0; j < 16; ++j)
      for (int k = 0; k < 8; ++k) kernels[i * 16 + j][k] = ker_arr[i][j * 8 + k];
}

size_t ref_wedge_lut(uint8_t *lut, size_t capacity, int offsets[22][2]) {
  /* The wedge masks as av1_compute.cpp writes them into inter_mask_lut: per block size the 16 masks of
   * either sign in full resolution, then, per block size again, the same masks for 4:2:0 chroma. The
   * interintra masks the decoder stores between them are not needed here; each list still starts on a
   * multiple of 64 bytes, which is the unit of the offsets. */
  size_t offset = 0;
  memset(lut, 0, capacity);
  for (int bsize = 0; bsize < BLOCK_SIZES_ALL; ++bsize) {
    const wedge_params_type *wedge_params = &wedge_params_lookup[bsize];
    offsets[bsize][0] = offsets[bsize][1] = 0;
    if (!wedge_params->bits) continue;
    const int size = block_size_wide[bsize] * block_size_high[bsize];
    offset = (offset + 63) & ~(size_t)63;
    if (offset + (size_t)size * 32 > capacity) abort();
    offsets[bsize][0] = (int)(offset >> 6);
    for (int index = 0; index < 16; ++index) {
      memcpy(lut + offset, wedge_params->masks[0][index], size);
      offset += size;
      memcpy(lut + offset, wedge_params->masks[1][index], size);
      offset += size;
    }
  }
  for (int bsize = 0; bsize < BLOCK_SIZES_ALL; ++bsize) {
    const wedge_params_type *wedge_params = &wedge_params_lookup[bsize];
    if (!wedge_params->bits) continue;
    const int bw = block_size_wide[bsize] >> 1;
    const int bh = block_size_high[bsize] >> 1;
    const int size = AOMMAX(64, bw * bh);
    offset = (offset + 63) & ~(size_t)63;
    if (offset + (size_t)size * 32 > capacity) abort();
    offsets[bsize][1] = (int)(offset >> 6);
    for (int index = 0; index < 16; ++index) {
      for (int sign = 0; sign < 2; ++sign) {
        int idx = 0;
        for (int y = 0; y < bh; ++y) {
          for (int x = 0; x < bw; ++x) {
            lut[offset + idx] = (wedge_params->masks[sign][index][x * 2 + (y * 2 + 0) * bw * 2] +
                                 wedge_params->masks[sign][index][x * 2 + 1 + (y * 2 + 0) * bw * 2] +
                                 wedge_params->masks[sign][index][x * 2 + (y * 2 + 1) * bw * 2] +
                                 wedge_params->masks[sign][index][x * 2 + 1 + (y * 2 + 1) * bw * 2] + 2) >>
                                2;
            ++idx;
          }
        }
        offset += size;
      }
    }
  }
  return offset;
}

void ref_predict(const RefBlock *b) {
  static MACROBLOCKD xd;
  static MB_MODE_INFO mi;
  static YV12_BUFFER_CONFIG cur_buf;
  static WarpedMotionParams global_motion[REF_FRAMES];
  static CONV_BUF_TYPE pred0[MAX_SB_SQUARE];
  static CONV_BUF_TYPE pred1[MAX_SB_SQUARE];
  static uint16_t unused_pixels[MAX_SB_SQUARE];

  const int hbd = b->bit_depth > 8;
  const int plane = b->plane;
  const int ss = plane > 0;
  const int is_compound = b->refs == 2;
  const BLOCK_SIZE bsize = (BLOCK_SIZE)ref_block_size(b->luma_w, b->luma_h);
  const int w = b->luma_w >> ss;
  const int h = b->luma_h >> ss;
  const int p_col = (b->mi_col * MI_SIZE) >> ss;
  const int p_row = (b->mi_row * MI_SIZE) >> ss;

  memset(&xd, 0, sizeof(xd));
  memset(&mi, 0, sizeof(mi));
  memset(&cur_buf, 0, sizeof(cur_buf));
  for (int i = 0; i < REF_FRAMES; ++i) global_motion[i] = default_warp_params;
  cur_buf.flags = hbd ? YV12_FLAG_HIGHBITDEPTH : 0;
  xd.cur_buf = &cur_buf;
  xd.bd = b->bit_depth;
  xd.global_motion = global_motion;
  xd.cur_frame_force_integer_mv = 0;
  xd.plane[plane].subsampling_x = ss;
  xd.plane[plane].subsampling_y = ss;

  mi.sb_type = bsize;
  mi.ref_frame[0] = LAST_FRAME;
  mi.ref_frame[1] = is_compound ? ALTREF_FRAME : NONE_FRAME;
  mi.mode = is_compound ? GLOBAL_GLOBALMV : (b->side[0].kind == REF_SIDE_LOCAL_WARP ? NEWMV : GLOBALMV);
  mi.motion_mode = b->side[0].kind == REF_SIDE_LOCAL_WARP ? WARPED_CAUSAL : SIMPLE_TRANSLATION;
  mi.interp_filters = av1_make_interp_filters((InterpFilter)b->filter_y, (InterpFilter)b->filter_x);
  mi.interinter_comp.type = b->comp_type == REF_COMP_WEDGE      ? COMPOUND_WEDGE
                            : b->comp_type == REF_COMP_DIFFWTD  ? COMPOUND_DIFFWTD
                            : b->comp_type == REF_COMP_DISTANCE ? COMPOUND_DISTWTD
                                                                : COMPOUND_AVERAGE;
  mi.interinter_comp.wedge_index = b->wedge_index;
  mi.interinter_comp.wedge_sign = b->wedge_sign;
  mi.interinter_comp.mask_type = b->mask_type;

  uint8_t *const dst = hbd ? CONVERT_TO_BYTEPTR(b->dst) : (uint8_t *)b->dst;
  const int masked = is_compound && (b->comp_type == REF_COMP_WEDGE || b->comp_type == REF_COMP_DIFFWTD);

  for (int ref = 0; ref < b->refs; ++ref) {
    const RefSide *const s = &b->side[ref];
    /* is_global_mv_block() and the motion mode, as dec_build_inter_predictors() fills them in. */
    const WarpTypesAllowed warp_types = {s->kind == REF_SIDE_GLOBAL_WARP, s->kind == REF_SIDE_LOCAL_WARP};
    if (s->kind != REF_SIDE_MV) {
      WarpedMotionParams wm;
      set_model(&wm, s->mat);
      if (!get_shear_params(&wm)) abort(); /* the check only sends models libaom accepts */
      if (s->kind == REF_SIDE_GLOBAL_WARP)
        global_motion[mi.ref_frame[ref]] = wm;
      else
        mi.wm_params = wm;
    }

    struct buf_2d *const pre_buf = &xd.plane[plane].pre[ref];
    pre_buf->buf0 = hbd ? CONVERT_TO_BYTEPTR(s->origin) : (uint8_t *)s->origin;
    pre_buf->buf = pre_buf->buf0;
    pre_buf->width = s->width;
    pre_buf->height = s->height;
    pre_buf->stride = s->stride;

    ConvolveParams conv_params = get_conv_params_no_round(ref, plane, pred0, MAX_SB_SIZE, is_compound, b->bit_depth);
    conv_params.use_dist_wtd_comp_avg = is_compound && b->comp_type == REF_COMP_DISTANCE;
    conv_params.fwd_offset = b->fwd_offset;
    conv_params.bck_offset = b->bck_offset;

    /* The unscaled branch of dec_calc_subpel_params(): the vector in 1/16 sample of the plane. */
    const int mv_row_q4 = s->mv_row * (1 << (1 - ss));
    const int mv_col_q4 = s->mv_col * (1 << (1 - ss));
    const SubpelParams subpel_params = {SCALE_SUBPEL_SHIFTS, SCALE_SUBPEL_SHIFTS,
                                        (mv_col_q4 & SUBPEL_MASK) << SCALE_EXTRA_BITS,
                                        (mv_row_q4 & SUBPEL_MASK) << SCALE_EXTRA_BITS};
    const uint8_t *const src =
        pre_buf->buf0 + (p_row + (mv_row_q4 >> SUBPEL_BITS)) * s->stride + p_col + (mv_col_q4 >> SUBPEL_BITS);

    if (ref && masked) {
      /* av1_make_masked_inter_predictor(): the second prediction goes to a buffer of its own, the mask
       * decides between the two. */
      conv_params.do_average = 0;
      conv_params.dst = pred1;
      conv_params.dst_stride = MAX_SB_SIZE;
      uint8_t *const unused = hbd ? CONVERT_TO_BYTEPTR(unused_pixels) : (uint8_t *)unused_pixels;
      av1_make_inter_predictor(src, s->stride, unused, MAX_SB_SIZE, &subpel_params, &g_unscaled, w, h, &conv_params,
                               mi.interp_filters, &warp_types, p_col, p_row, plane, ref, &mi, 0, &xd, 1);
      if (!plane && b->comp_type == REF_COMP_DIFFWTD)
        av1_build_compound_diffwtd_mask_d16_c(b->seg_mask, (DIFFWTD_MASK_TYPE)b->mask_type, pred0, MAX_SB_SIZE, pred1,
                                              MAX_SB_SIZE, h, w, &conv_params, b->bit_depth);
      /* build_masked_compound_no_round() */
      const int subh = (2 << mi_size_high_log2[bsize]) == h;
      const int subw = (2 << mi_size_wide_log2[bsize]) == w;
      const uint8_t *const mask = b->comp_type == REF_COMP_WEDGE
                                      ? av1_get_contiguous_soft_mask(b->wedge_index, b->wedge_sign, bsize)
                                      : b->seg_mask;
      if (hbd)
        aom_highbd_blend_a64_d16_mask_c(dst, b->dst_stride, pred0, MAX_SB_SIZE, pred1, MAX_SB_SIZE, mask,
                                        block_size_wide[bsize], w, h, subw, subh, &conv_params, b->bit_depth);
      else
        aom_lowbd_blend_a64_d16_mask_c(dst, b->dst_stride, pred0, MAX_SB_SIZE, pred1, MAX_SB_SIZE, mask,
                                       block_size_wide[bsize], w, h, subw, subh, &conv_params);
    } else {
      av1_make_inter_predictor(src, s->stride, dst, b->dst_stride, &subpel_params, &g_unscaled, w, h, &conv_params,
                               mi.interp_filters, &warp_types, p_col, p_row, plane, ref, &mi, 0, &xd, 1);
    }
  }
}
