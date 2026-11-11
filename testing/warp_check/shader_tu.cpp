/*
 * One shader as a translation unit. run.sh builds this file once per shader:
 *   SHADER_FILE  the generated C++ of the shader
 *   SHADER_NS    namespace for its globals
 *   SHADER_API   name of the exported ShaderApi
 *   SHADER_KIND  0 = inter_warp, 1 = inter_scale_warp_compound, 2 = inter_ext_borders
 */
#include "hlsl_shim.h"
#include "shader_api.h"

namespace SHADER_NS {

#include SHADER_FILE

static void bind(const ShaderBuffers *b) {
#if SHADER_KIND == 2
  Dst.ptr = b->frame;
  Dst.size = b->frame_size;
#else
  pred_blocks.ptr = b->pred_blocks;
  pred_blocks.size = b->pred_blocks_size;
  warp_blocks.ptr = b->warp_blocks;
  warp_blocks.size = b->warp_blocks_size;
  residuals.ptr = b->residuals;
  residuals.size = b->residuals_size;
  comp_mask.ptr = b->comp_mask;
  comp_mask.size = b->comp_mask_size;
  warp_filter.ptr = b->warp_filter;
  warp_filter.size = b->warp_filter_size;
  dst_frame.ptr = b->frame;
  dst_frame.size = b->frame_size;
#endif
}

static void set_common(const void *consts) {
#if SHADER_KIND == 2
  const BorderConsts *c = (const BorderConsts *)consts;
  static_assert(sizeof(cb_planes) == sizeof(c->planes), "planes");
  static_assert(sizeof(cb_dims) == sizeof(c->dims), "dims");
  memcpy((void *)cb_planes, c->planes, sizeof(cb_planes));
  memcpy((void *)cb_dims, c->dims, sizeof(cb_dims));
#else
  const InterConsts *c = (const InterConsts *)consts;
  static_assert(sizeof(cb_planes) == sizeof(c->planes), "planes");
  static_assert(sizeof(cb_refplanes) == sizeof(c->refplanes), "reference planes");
  static_assert(sizeof(cb_dims) == sizeof(c->dims), "dims");
  static_assert(sizeof(cb_pixel_max) == sizeof(c->pixel_max), "pixel max");
  static_assert(sizeof(cb_kernels) == sizeof(c->kernels), "kernels");
  static_assert(sizeof(cb_scale) == sizeof(c->scale), "scale");
  static_assert(sizeof(cb_obmc_mask) == sizeof(c->obmc_mask), "obmc mask");
  static_assert(sizeof(cb_gm_warp) == sizeof(c->warp), "global motion");
  memcpy((void *)cb_planes, c->planes, sizeof(cb_planes));
  memcpy((void *)cb_refplanes, c->refplanes, sizeof(cb_refplanes));
  memcpy((void *)cb_dims, c->dims, sizeof(cb_dims));
  memcpy((void *)&cb_pixel_max, c->pixel_max, sizeof(cb_pixel_max));
  memcpy((void *)cb_kernels, c->kernels, sizeof(cb_kernels));
  memcpy((void *)cb_scale, c->scale, sizeof(cb_scale));
  memcpy((void *)cb_obmc_mask, c->obmc_mask, sizeof(cb_obmc_mask));
  memcpy((void *)cb_gm_warp, c->warp, sizeof(cb_gm_warp));
#endif
}

static void set_srt(const uint32_t *w) {
#if SHADER_KIND == 2
  (void)w;
#else
  /* PSSLInterSRT of prediction.cpp. */
  cb_wi_count = w[0];
  cb_pass_offset = w[1];
  cb_width_log2 = w[2];
  cb_height_log2 = w[3];
#endif
}

static void scramble(uint64_t *state) {
#if SHADER_KIND == 0
  uint8_t *bytes = (uint8_t *)mem;
  const size_t size = sizeof(mem);
#elif SHADER_KIND == 1
  uint8_t *bytes = (uint8_t *)intermediate_buffer;
  const size_t size = sizeof(intermediate_buffer);
#else
  uint8_t *bytes = nullptr;
  const size_t size = 0;
#endif
  for (size_t i = 0; i + 4 <= size; i += 4) {
    *state = *state * 6364136223846793005ull + 1442695040888963407ull;
    const uint32_t v = (uint32_t)(*state >> 32);
    memcpy(bytes + i, &v, 4);
  }
}

#if SHADER_KIND == 2
static int (*const scale_value_fn)(int, int) = nullptr;
#else
static int scale_value_fn(int val, int scale) { return scale_value(val, scale); }
#endif

static void run(uint32_t thread) { shader_main(uint3(thread, 0, 0)); /* numthreads(64, 1, 1), SV_DispatchThreadID */ }

}  // namespace SHADER_NS

extern const ShaderApi SHADER_API = {
    64,
    SHADER_NS::bind,
    SHADER_NS::set_common,
    SHADER_NS::set_srt,
    SHADER_NS::scramble,
    SHADER_NS::run,
    SHADER_NS::scale_value_fn,
};
