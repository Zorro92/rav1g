// SPDX-License-Identifier: BSD-2-Clause
#pragma once

#include "hlsl_shim.h"

struct FilterDispatch {
  RWByteAddressBuffer frame;
  ByteAddressBuffer edges;
  int4 planes[3];
  int4 limits[64];
  uint work_items, plane, list_base, block_cols, block_offset;
};

struct FilterKernel {
  const char *name;
  void (*bind)(const FilterDispatch &);
  void (*entry)(int, void *);
};

extern const FilterKernel loopfilter_vert_hbd;
extern const FilterKernel loopfilter_vert_hbd_legacy;
extern const FilterKernel loopfilter_hor_hbd;
extern const FilterKernel loopfilter_hor_hbd_legacy;

#include "mode_info.h"

struct GenDispatch {
  StructuredBuffer<MB_MODE_INFO> pool;
  ByteAddressBuffer grid;
  RWByteAddressBuffer edges;
  uint mi_stride, mi_addr_base, delta_present;
  int delta_multi, mode_ref_delta;
  uint mi_grid_last;
  int2 filter_level;
  int filter_level_u, filter_level_v;
  int4 mode_deltas[2], ref_deltas[8], seg_features[8], seg_data[64], levels[3 * 8 * 8 * 2];
  uint work_items, mi_cols, mi_rows, plane, dst_offset, dst_stride;
};

struct GenKernel {
  const char *name;
  void (*bind)(const GenDispatch &);
  void (*entry)(int, void *);
};

extern const GenKernel gen_lf_blocks_vert;
extern const GenKernel gen_lf_blocks_vert_legacy;
extern const GenKernel gen_lf_blocks_hor;
extern const GenKernel gen_lf_blocks_hor_legacy;
