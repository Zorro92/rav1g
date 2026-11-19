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
