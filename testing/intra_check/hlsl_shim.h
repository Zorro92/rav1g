/*
 * Just enough HLSL for the intra prediction shaders (intra_main, intra_filter,
 * intra_v2 and their _hbd builds) to compile as C++ after run.sh has stripped
 * attributes, semantics and register bindings. Integer maths wraps like on
 * the GPU: build with -fwrapv.
 */
#ifndef INTRA_CHECK_HLSL_SHIM_H_
#define INTRA_CHECK_HLSL_SHIM_H_

#include <stdint.h>
#include <stdlib.h>
#include <string.h>

typedef unsigned int uint;

struct uint2 {
  uint x, y;
  uint2() : x(0), y(0) {}
  uint2(uint v) : x(v), y(v) {}
  uint2(uint a, uint b) : x(a), y(b) {}
};
struct int2 {
  int x, y;
  int2() : x(0), y(0) {}
  int2(int v) : x(v), y(v) {}
  int2(int a, int b) : x(a), y(b) {}
  int2(const uint2 &v) : x((int)v.x), y((int)v.y) {}
};
struct uint3 {
  uint x, y, z;
  uint3() : x(0), y(0), z(0) {}
  uint3(uint v) : x(v), y(v), z(v) {}
  uint3(uint a, uint b, uint c) : x(a), y(b), z(c) {}
};
struct int3 {
  int x, y, z;
  int3() : x(0), y(0), z(0) {}
  int3(int a, int b, int c) : x(a), y(b), z(c) {}
};
struct uint4 {
  uint x, y, z, w;
  uint4() : x(0), y(0), z(0), w(0) {}
  uint4(uint a, uint b, uint c, uint d) : x(a), y(b), z(c), w(d) {}
};
struct int4 {
  int x, y, z, w;
  int4() : x(0), y(0), z(0), w(0) {}
  int4(int v) : x(v), y(v), z(v), w(v) {}
  int4(int a, int b, int c, int d) : x(a), y(b), z(c), w(d) {}
  int3 xyz() const { return int3(x, y, z); }
  int4 &operator<<=(int s) {
    x <<= s;
    y <<= s;
    z <<= s;
    w <<= s;
    return *this;
  }
};

static inline int min(int a, int b) { return a < b ? a : b; }
static inline int max(int a, int b) { return a > b ? a : b; }
static inline int clamp(int v, int lo, int hi) { return min(max(v, lo), hi); }

/* Counts accesses outside the bound buffer. */
extern long g_out_of_bounds;

/*
 * Watches the frame buffer for reads that depend on the order in which the
 * GPU runs things:
 *   - a word read by one thread group that another group wrote since the last
 *     UAV barrier ("unit");
 *   - a word read by one thread that another thread of its group wrote since
 *     the last group sync ("phase"), where the shader relies on syncs.
 * The tags are one per 32-bit word of the buffer.
 */
struct AccessTracker {
  uint32_t *w_unit = nullptr;
  uint32_t *w_group = nullptr;
  uint32_t *w_phase = nullptr;
  uint32_t *w_thread = nullptr;
  uint32_t unit = 1, group = 0, phase = 1, thread = 0;
  int track_phase = 0;
  long races_group = 0, races_thread = 0;
};
extern AccessTracker g_tracker;

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
  uint3 Load3(uint address) const { return uint3(Load(address), Load(address + 4), Load(address + 8)); }
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
    if (g_tracker.w_unit) {
      const size_t word = address >> 2;
      if (g_tracker.w_unit[word] == g_tracker.unit && g_tracker.w_group[word] != g_tracker.group)
        g_tracker.races_group++;
      else if (g_tracker.track_phase && g_tracker.w_phase[word] == g_tracker.phase &&
               g_tracker.w_thread[word] != g_tracker.thread)
        g_tracker.races_thread++;
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
    if (g_tracker.w_unit) {
      const size_t word = address >> 2;
      g_tracker.w_unit[word] = g_tracker.unit;
      g_tracker.w_group[word] = g_tracker.group;
      g_tracker.w_phase[word] = g_tracker.phase;
      g_tracker.w_thread[word] = g_tracker.thread;
    }
    memcpy(ptr + address, &v, 4);
  }
  void Store2(uint address, uint2 v) {
    Store(address, v.x);
    Store(address + 4, v.y);
  }
};

/*
 * Scheduling points of the emulated thread group, see intra_check.cpp.
 * GroupMemoryBarrier() has no ordering effect on a GPU; the original shaders
 * use it where they rely on the threads of a wave running in lockstep, so
 * here it lets every other runnable thread reach its next barrier. WaveJoin()
 * is inserted by run.sh where lockstep execution makes all threads meet (the
 * end of a loop some threads leave early).
 */
void GroupMemoryBarrier();
void GroupMemoryBarrierWithGroupSync();
void AllMemoryBarrierWithGroupSync();
void WaveJoin(int level);

#define groupshared static

#endif /* INTRA_CHECK_HLSL_SHIM_H_ */
