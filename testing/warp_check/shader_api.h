/*
 * What warp_check.cpp sees of one shader. Every shader is compiled as its own
 * translation unit (shader_tu.cpp), in its own namespace.
 */
#ifndef WARP_CHECK_SHADER_API_H_
#define WARP_CHECK_SHADER_API_H_

#include <stddef.h>
#include <stdint.h>

/* The constant buffer PSSLInterData of the inter shaders, laid out like the struct of that name in prediction.cpp. */
struct InterConsts {
  int planes[3][4];        /* stride, offset, residual stride, residual offset */
  int refplanes[3 * 7][4]; /* stride, offset, width - 1, height - 1 */
  int dims[2][4];          /* width - 1, height - 1 of luma and of chroma */
  int pixel_max[4];
  int kernels[8 * 16][8];
  int scale[8][4]; /* x scale, x step, y scale, y step; entry 1 + reference */
  int obmc_mask[16][4];
  int warp[7][12]; /* is_warp, reserved, mat[6], alpha, beta, delta, gamma */
};

/* The constant buffer of inter_ext_borders: CBData of av1_inter_ext_borders() in prediction.cpp. */
struct BorderConsts {
  int planes[3][4];
  int dims[2][4];
};

struct ShaderBuffers {
  const uint8_t *pred_blocks; /* 16 bytes per block */
  size_t pred_blocks_size;
  const uint8_t *warp_blocks; /* 48 bytes per block */
  size_t warp_blocks_size;
  const uint8_t *residuals;
  size_t residuals_size;
  const uint8_t *comp_mask; /* the wedge masks */
  size_t comp_mask_size;
  const uint8_t *warp_filter;
  size_t warp_filter_size;
  uint8_t *frame; /* the frame pool */
  size_t frame_size;
};

struct ShaderApi {
  int threads; /* threads per group */
  void (*bind)(const ShaderBuffers *buffers);
  void (*set_common)(const void *consts); /* InterConsts, or BorderConsts for the border shaders */
  void (*set_srt)(const uint32_t *words); /* the root constants, as the host sets them */
  void (*scramble)(uint64_t *state);      /* fills the group memory with garbage */
  void (*run)(uint32_t thread);           /* SV_DispatchThreadID.x */
  int (*scale_value)(int val, int scale); /* scale_value() of inter_common.h; null for the border shaders */
};

extern const ShaderApi inter_warp_api;
extern const ShaderApi inter_warp_hbd_api;
extern const ShaderApi inter_warp_comp_api;
extern const ShaderApi inter_warp_comp_hbd_api;
extern const ShaderApi inter_ext_borders_api;
extern const ShaderApi inter_ext_borders_hbd_api;

#endif /* WARP_CHECK_SHADER_API_H_ */
