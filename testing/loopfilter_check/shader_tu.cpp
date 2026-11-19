// SPDX-License-Identifier: BSD-2-Clause
#include "shader_adapter.h"

namespace {
#include SHADER_SOURCE

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

void run(int lane, void *base) {
  shader_main(uint3{*static_cast<const uint *>(base) + uint(lane), 0, 0});
}
}

#define STRING_IMPL(x) #x
#define STRING(x) STRING_IMPL(x)
extern const FilterKernel ENTRY = {STRING(ENTRY), bind, run};
