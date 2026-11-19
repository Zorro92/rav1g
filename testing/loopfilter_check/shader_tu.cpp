// SPDX-License-Identifier: BSD-2-Clause
#include "shader_adapter.h"

namespace {
#include SHADER_SOURCE

#ifdef GEN_SHADER
void bind(const GenDispatch &d) {
  mi_pool = d.pool;
  mi_grid = d.grid;
  dst_buf = d.edges;
  cb_mi_stride = d.mi_stride;
  cb_mi_addr_base = d.mi_addr_base;
  cb_delta_q_info_delta_lf_present_flag = d.delta_present;
  cb_delta_q_info_delta_lf_multi = d.delta_multi;
  cb_lf_mode_ref_delta_enabled = d.mode_ref_delta;
  cb_mi_grid_entry = 4;
  cb_mi_grid_last = d.mi_grid_last;
  cb_lf_filter_level = d.filter_level;
  cb_lf_filter_level_u = d.filter_level_u;
  cb_lf_filter_level_v = d.filter_level_v;
  std::memcpy(cb_lf_mode_deltas, d.mode_deltas, sizeof(cb_lf_mode_deltas));
  std::memcpy(cb_lf_ref_deltas, d.ref_deltas, sizeof(cb_lf_ref_deltas));
  std::memcpy(cb_seg_features, d.seg_features, sizeof(cb_seg_features));
  std::memcpy(cb_seg_data, d.seg_data, sizeof(cb_seg_data));
  std::memcpy(cb_lfi_n_lvl, d.levels, sizeof(cb_lfi_n_lvl));
  cb_wicount = d.work_items;
  cb_mi_cols = d.mi_cols;
  cb_mi_rows = d.mi_rows;
  cb_plane = d.plane;
  cb_dst_offset = d.dst_offset;
  cb_dst_stride = d.dst_stride;
}
#else
void bind(const FilterDispatch &d) {
  dst_frame = d.frame;
  lf_blocks = d.edges;
  std::memcpy(cb_planes, d.planes, sizeof(cb_planes));
  std::memcpy(cb_limits, d.limits, sizeof(cb_limits));
  cb_wicount = d.work_items;
  cb_plane = d.plane;
  cb_offset_base = d.list_base;
  cb_block_cols = d.block_cols;
  cb_block_id_offset = d.block_offset;
}

#endif

void run(int lane, void *base) {
  shader_main(uint3{*static_cast<const uint *>(base) + uint(lane), 0, 0});
}
}

#define STRING_IMPL(x) #x
#define STRING(x) STRING_IMPL(x)
#ifdef GEN_SHADER
extern const GenKernel ENTRY = {STRING(ENTRY), bind, run};
#else
extern const FilterKernel ENTRY = {STRING(ENTRY), bind, run};
#endif
