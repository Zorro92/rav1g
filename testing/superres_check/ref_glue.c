/*
 * The reference side of superres_check. See ref_glue.h.
 */
#include <stdlib.h>
#include <string.h>

#include "config/aom_config.h"

#include "av1/common/onyxc_int.h"
#include "av1/common/resize.h"
#include "av1/common/tile_common.h"

#include "ref_glue.h"

int ref_downscaled_width(int upscaled_width, int denominator) {
  int width = upscaled_width;
  int height = 16;
  av1_calculate_scaled_superres_size(&width, &height, denominator);
  return width;
}

void ref_upscale_plane(const RefUpscale *u, const void *src, int src_stride, void *dst, int dst_stride) {
  /* Too large for the stack, and only a few of its fields are read by the upscale. */
  static AV1_COMMON cm;
  memset(&cm, 0, sizeof(cm));
  cm.superres_upscaled_width = u->upscaled_width;
  cm.superres_scale_denominator = (uint8_t)u->denominator;
  cm.width = ref_downscaled_width(u->upscaled_width, u->denominator);
  cm.mi_cols = ((cm.width + 7) >> 3) << 1;
  cm.seq_params.subsampling_x = 1;
  cm.seq_params.subsampling_y = 1;
  cm.seq_params.use_highbitdepth = (uint8_t)u->use_highbitdepth;
  cm.seq_params.bit_depth = (aom_bit_depth_t)u->bit_depth;
  cm.seq_params.mib_size_log2 = u->sb_size_log2 - 2;
  cm.tile_cols = u->tile_cols;
  for (int i = 0; i <= u->tile_cols; ++i) cm.tile_col_start_sb[i] = u->tile_col_start_sb[i];
  if (u->use_highbitdepth)
    av1_upscale_normative_rows(&cm, CONVERT_TO_BYTEPTR((const uint16_t *)src), src_stride,
                               CONVERT_TO_BYTEPTR((uint16_t *)dst), dst_stride, u->plane, u->rows);
  else
    av1_upscale_normative_rows(&cm, (const uint8_t *)src, src_stride, (uint8_t *)dst, dst_stride, u->plane, u->rows);
}
