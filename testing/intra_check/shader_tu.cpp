/*
 * One shader as a translation unit. run.sh builds this file once per shader:
 *   SHADER_FILE  the generated C++ of the shader
 *   SHADER_NS    namespace for its globals
 *   SHADER_API   name of the exported ShaderApi
 *   SHADER_KIND  0 = intra_main, 1 = intra_filter, 2 = intra_v2
 */
#include "hlsl_shim.h"
#include "shader_api.h"

namespace SHADER_NS {

#include SHADER_FILE

static void bind(const ShaderBuffers *b) {
  pred_blocks.ptr = b->pred_blocks;
  pred_blocks.size = b->pred_blocks_size;
  residuals.ptr = b->residuals;
  residuals.size = b->residuals_size;
  wedge_mask.ptr = b->wedge;
  wedge_mask.size = b->wedge_size;
  dst_frame.ptr = b->frame;
  dst_frame.size = b->frame_size;
#if SHADER_KIND == 2
  sched.ptr = b->sched;
  sched.size = b->sched_size;
  block_map.ptr = b->block_map;
  block_map.size = b->block_map_size;
#endif
}

static void set_common(const IntraConsts *c) {
  static_assert(sizeof(cb_planes) == sizeof(c->planes), "planes");
  static_assert(sizeof(cb_flags) == sizeof(c->flags), "flags");
  static_assert(sizeof(cb_filter) == sizeof(c->filter), "filter");
  static_assert(sizeof(cb_mode_params_lut) == sizeof(c->mode_params_lut), "mode params");
  static_assert(sizeof(cb_sm_weight_arrays) == sizeof(c->sm_weight_arrays), "smooth weights");
  memcpy((void *)cb_planes, c->planes, sizeof(cb_planes));
  memcpy((void *)&cb_flags, c->flags, sizeof(cb_flags));
  memcpy((void *)cb_filter, c->filter, sizeof(cb_filter));
  memcpy((void *)cb_mode_params_lut, c->mode_params_lut, sizeof(cb_mode_params_lut));
  memcpy((void *)cb_sm_weight_arrays, c->sm_weight_arrays, sizeof(cb_sm_weight_arrays));
}

static void set_srt(const uint32_t *w) {
#if SHADER_KIND == 0
  /* PSSLIntraSRT of prediction.cpp: counts0[8], wi_count, pass_offset. */
  memcpy((void *)&cb_counts0, w, 16);
  memcpy((void *)&cb_counts1, w + 4, 16);
  cb_wi_count = w[8];
  cb_pass_offset = (int)w[9];
#elif SHADER_KIND == 1
  cb_wi_count = w[0];
  cb_pass_offset = (int)w[1];
#else
  cb_list_offset = w[0];
  cb_step = w[1];
  cb_passes = w[2];
  cb_stamp = w[3];
  cb_map_stride = w[4];
  cb_map_uv_offset = w[5];
  cb_reserved0 = w[6];
  cb_reserved1 = w[7];
#endif
}

static void garbage(void *p, size_t size, uint64_t *state) {
  uint8_t *bytes = (uint8_t *)p;
  for (size_t i = 0; i + 4 <= size; i += 4) {
    *state = *state * 6364136223846793005ull + 1442695040888963407ull;
    const uint32_t v = (uint32_t)(*state >> 32);
    memcpy(bytes + i, &v, 4);
  }
}

static void scramble(uint64_t *state) {
#if SHADER_KIND == 0
  garbage(above, sizeof(above), state);
  garbage(left, sizeof(left), state);
#elif SHADER_KIND == 1
  garbage(mem, sizeof(mem), state);
  garbage(loc_above, sizeof(loc_above), state);
  garbage(loc_left, sizeof(loc_left), state);
  garbage(loc_corner, sizeof(loc_corner), state);
#else
  garbage(gs_sum_above, sizeof(gs_sum_above), state);
  garbage(gs_sum_left, sizeof(gs_sum_left), state);
  garbage(gs_sum_luma, sizeof(gs_sum_luma), state);
  garbage(gs_fi_bottom, sizeof(gs_fi_bottom), state);
  garbage(gs_fi_right, sizeof(gs_fi_right), state);
#endif
}

static void run(uint32_t group, uint32_t thread) {
#if SHADER_KIND == 0
  shader_main(uint3(group * 256 + thread, 0, 0)); /* numthreads(256, 1, 1), SV_DispatchThreadID */
#elif SHADER_KIND == 1
  shader_main(uint3(group * 8 + (thread & 7), thread >> 3, 0)); /* numthreads(8, 8, 1), SV_DispatchThreadID */
#else
  shader_main(uint3(group, 0, 0), uint3(thread, 0, 0)); /* SV_GroupID, SV_GroupThreadID */
#endif
}

}  // namespace SHADER_NS

#if SHADER_KIND == 0
#define SHADER_THREADS 256
#elif SHADER_KIND == 1
#define SHADER_THREADS 64
#else
#define SHADER_THREADS 384
#endif

extern const ShaderApi SHADER_API = {
  SHADER_THREADS, SHADER_NS::bind, SHADER_NS::set_common, SHADER_NS::set_srt, SHADER_NS::scramble, SHADER_NS::run,
};
