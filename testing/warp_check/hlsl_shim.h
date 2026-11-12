/*
 * Just enough HLSL for the warp inter prediction shaders (inter_warp,
 * inter_scale_warp_compound, their _hbd builds, inter_ext_borders) to compile
 * as C++ after run.sh has stripped attributes, semantics and register
 * bindings. Integer maths wraps like on the GPU: build with -fwrapv.
 */
#ifndef WARP_CHECK_HLSL_SHIM_H_
#define WARP_CHECK_HLSL_SHIM_H_

#include <stdint.h>
#include <stdlib.h>
#include <string.h>

typedef unsigned int uint;

struct uint2 {
  uint x, y;
  uint2() : x(0), y(0) {}
  uint2(uint a, uint b) : x(a), y(b) {}
};
struct int2 {
  int x, y;
  int2() : x(0), y(0) {}
  int2(int a, int b) : x(a), y(b) {}
  int2(const uint2 &v) : x((int)v.x), y((int)v.y) {}
};
struct uint3 {
  uint x, y, z;
  uint3() : x(0), y(0), z(0) {}
  uint3(uint a, uint b, uint c) : x(a), y(b), z(c) {}
};
struct uint4 {
  uint x, y, z, w;
  uint4() : x(0), y(0), z(0), w(0) {}
  uint4(uint a, uint b, uint c, uint d) : x(a), y(b), z(c), w(d) {}
};
struct int4 {
  int x, y, z, w;
  int4() : x(0), y(0), z(0), w(0) {}
  int4(int a, int b, int c, int d) : x(a), y(b), z(c), w(d) {}
  int4(const uint4 &v) : x((int)v.x), y((int)v.y), z((int)v.z), w((int)v.w) {}
  int2 xy() const { return int2(x, y); }
};
static inline int4 operator-(const int4 &a, const int4 &b) { return int4(a.x - b.x, a.y - b.y, a.z - b.z, a.w - b.w); }
// The row addresses of the warp shaders: a scalar plus a scalar times a clamped vector.
struct int3 {
  int x, y, z;
  int3() : x(0), y(0), z(0) {}
  int3(int a, int b, int c) : x(a), y(b), z(c) {}
};
static inline int4 operator+(int a, const int4 &b) { return int4(a + b.x, a + b.y, a + b.z, a + b.w); }
static inline int4 operator*(int a, const int4 &b) { return int4(a * b.x, a * b.y, a * b.z, a * b.w); }
static inline int3 operator+(int a, const int3 &b) { return int3(a + b.x, a + b.y, a + b.z); }
static inline int3 operator*(int a, const int3 &b) { return int3(a * b.x, a * b.y, a * b.z); }

static inline int min(int a, int b) { return a < b ? a : b; }
static inline int max(int a, int b) { return a > b ? a : b; }
static inline int clamp(int v, int lo, int hi) { return min(max(v, lo), hi); }
static inline int4 clamp(const int4 &v, int lo, int hi) {
  return int4(clamp(v.x, lo, hi), clamp(v.y, lo, hi), clamp(v.z, lo, hi), clamp(v.w, lo, hi));
}
static inline int3 clamp(const int3 &v, int lo, int hi) {
  return int3(clamp(v.x, lo, hi), clamp(v.y, lo, hi), clamp(v.z, lo, hi));
}

/* Counts accesses outside the bound buffer; any is a failure of the check. */
extern long g_out_of_bounds;

/*
 * Double precision as the shader sees it. run.sh turns every "double" of the
 * shader sources into this type. With g_double_model 1 it is IEEE double, as
 * on a GPU that executes the double-precision instructions FXC emits (itod,
 * dmul, dadd, dtoi). With g_double_model 0 every operation yields 0, which is
 * what the console's output corresponds to (see docs/gpu-av1-decoder.md,
 * "Global motion"). g_double_ops counts the operations either way: shaders
 * that the check passes must not execute any.
 */
extern int g_double_model;
extern long g_double_ops;
struct gpu_double {
  double v;
  gpu_double() : v(0) {}
  gpu_double(double d) : v(d) {
    g_double_ops++;
    if (!g_double_model) v = 0;
  }
  explicit operator int() const {
    g_double_ops++;
    return g_double_model ? (int)v : 0;
  }
  gpu_double &operator*=(double d) {
    *this = gpu_double(v * d);
    return *this;
  }
};
static inline gpu_double operator*(const gpu_double &a, int b) { return gpu_double(a.v * b); }
static inline gpu_double operator+(const gpu_double &a, int b) { return gpu_double(a.v + b); }
static inline gpu_double operator-(const gpu_double &a, int b) { return gpu_double(a.v - b); }
static inline bool operator<(const gpu_double &a, int b) {
  g_double_ops++;
  return g_double_model ? a.v < b : false;
}

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
  uint2 Load2(uint address) const { return uint2(Load(address), Load(address + 4)); }
  uint4 Load4(uint address) const {
    return uint4(Load(address), Load(address + 4), Load(address + 8), Load(address + 12));
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
  uint2 Load2(uint address) const { return uint2(Load(address), Load(address + 4)); }
  uint3 Load3(uint address) const { return uint3(Load(address), Load(address + 4), Load(address + 8)); }
  uint4 Load4(uint address) const {
    return uint4(Load(address), Load(address + 4), Load(address + 8), Load(address + 12));
  }
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

#endif /* WARP_CHECK_HLSL_SHIM_H_ */
