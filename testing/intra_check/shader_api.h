/*
 * What intra_check.cpp sees of one shader. Every shader is compiled as its
 * own translation unit (shader_tu.cpp), in its own namespace.
 */
#ifndef INTRA_CHECK_SHADER_API_H_
#define INTRA_CHECK_SHADER_API_H_

#include <stddef.h>
#include <stdint.h>

/* The constant buffer IntraDataCommon, laid out like PSSLIntraData in prediction.cpp. */
struct IntraConsts {
  int planes[3][4]; /* stride, offset, residual stride, residual offset */
  int flags[4];
  int filter[5][8][8];
  int mode_params_lut[16][7][4];
  int sm_weight_arrays[128][4];
};

struct ShaderBuffers {
  const uint8_t *pred_blocks;
  size_t pred_blocks_size;
  const uint8_t *residuals;
  size_t residuals_size;
  const uint8_t *wedge;
  size_t wedge_size;
  const uint8_t *sched; /* intra_v2 only */
  size_t sched_size;
  const uint8_t *block_map; /* intra_v2 only */
  size_t block_map_size;
  uint8_t *frame;
  size_t frame_size;
};

struct ShaderApi {
  int threads; /* threads per group */
  void (*bind)(const ShaderBuffers *buffers);
  void (*set_common)(const IntraConsts *consts);
  void (*set_srt)(const uint32_t *words); /* the root constants, as the host sets them */
  void (*scramble)(uint64_t *state);      /* fills the group memory with garbage */
  void (*run)(uint32_t group, uint32_t thread);
};

extern const ShaderApi intra_main_api;
extern const ShaderApi intra_main_hbd_api;
extern const ShaderApi intra_filter_api;
extern const ShaderApi intra_filter_hbd_api;
extern const ShaderApi intra_v2_api;
extern const ShaderApi intra_v2_hbd_api;

#endif /* INTRA_CHECK_SHADER_API_H_ */
