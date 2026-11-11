/*
 * Just enough HLSL for cdef_filter.hlsl and cdef_filter_v2.hlsl to compile as
 * C++ (after run.sh has stripped attributes, semantics and register
 * bindings). Integer maths wraps like on the GPU: build with -fwrapv.
 */
#ifndef CDEF_CHECK_HLSL_SHIM_H_
#define CDEF_CHECK_HLSL_SHIM_H_

#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

typedef unsigned int uint;

struct int2 {
  int x, y;
};
struct uint2 {
  uint x, y;
};
struct int3 {
  int x, y, z;
};
struct uint3 {
  uint x, y, z;
};
union int4 {
  struct {
    int x, y, z, w;
  };
  struct {
    int2 xy;
    int2 zw;
  };
};

static inline int min(int a, int b) { return a < b ? a : b; }
static inline int max(int a, int b) { return a > b ? a : b; }
static inline int sign(int a) { return (a > 0) - (a < 0); }
static inline int clamp(int v, int lo, int hi) { return min(max(v, lo), hi); }

/* Counts accesses outside the bound buffer; any is a failure of the check. */
extern long g_out_of_bounds;

struct ByteAddressBuffer {
  const uint8_t *ptr = nullptr;
  size_t size = 0;
  uint Load(uint address) const {
    uint v = 0;
    if ((size_t)address + 4 > size) {
      g_out_of_bounds++;
      return 0;
    }
    memcpy(&v, ptr + address, 4);
    return v;
  }
};

struct RWByteAddressBuffer {
  uint8_t *ptr = nullptr;
  size_t size = 0;
  uint Load(uint address) const {
    uint v = 0;
    if ((size_t)address + 4 > size) {
      g_out_of_bounds++;
      return 0;
    }
    memcpy(&v, ptr + address, 4);
    return v;
  }
  uint2 Load2(uint address) const { return uint2{Load(address), Load(address + 4)}; }
  void Store(uint address, uint v) {
    if ((size_t)address + 4 > size) {
      g_out_of_bounds++;
      return;
    }
    memcpy(ptr + address, &v, 4);
  }
  void Store2(uint address, uint2 v) {
    Store(address, v.x);
    Store(address + 4, v.y);
  }
};

/*
 * The thread group and its group memory: testing/common/group_sched.h. A group sync is where the threads
 * of a group meet; GroupMemoryBarrier() makes no thread wait, here as on a GPU, and is counted. The group
 * memory (GsArray, see run.sh) records every read of what another thread wrote since the last group sync.
 */
#include "../common/group_sched.h"

static inline void GroupMemoryBarrier() { gs_state.plain_barriers++; }
static inline void GroupMemoryBarrierWithGroupSync() { gs_group_sync(); }

#endif /* CDEF_CHECK_HLSL_SHIM_H_ */
