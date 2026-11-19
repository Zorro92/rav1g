// SPDX-License-Identifier: BSD-2-Clause
#pragma once

#include <algorithm>
#include <bit>
#include <cstdint>
#include <cstdio>
#include <cstdlib>
#include <cstring>

#include "../common/group_sched.h"

using uint = unsigned int;
using std::clamp;
using std::max;
using std::min;

// HLSL resolves the legacy horizontal generator's mixed-sign clamp to an
// integer overload. The active strip always starts within the frame.
inline int clamp(uint v, int lo, int hi) { return std::clamp(int(v), lo, hi); }

inline uint max(uint a, GsScalarRef<uint> b) { return std::max(a, uint(b)); }

struct uint2 { uint x, y; };
struct uint3 { uint x, y, z; };
struct uint4 {
  uint x, y, z, w;
  uint operator[](uint i) const { return i < 2 ? (i == 0 ? x : y) : (i == 2 ? z : w); }
};
struct int2 { int x, y; };
struct int4 { int x, y, z, w; };

inline int4 operator<<(int4 v, int shift) {
  return {v.x << shift, v.y << shift, v.z << shift, v.w << shift};
}

inline uint firstbitlow(uint v) {
  return v == 0 ? ~0u : std::countr_zero(v);
}

inline void check_access(size_t word, uint address, size_t count) {
  if (word >= count || (address & 3) != 0) {
    std::fprintf(stderr, "FAIL: %s unaligned or out-of-bounds access at byte %u (size %zu)\n",
                 gs_state.shader, address, count * 4);
    std::exit(1);
  }
}

struct ByteAddressBuffer {
  const uint *words;
  size_t count;

  uint Load(uint address) const {
    const size_t word = address / 4;
    check_access(word, address, count);
    return words[word];
  }

  uint2 Load2(uint address) const { return {Load(address), Load(address + 4)}; }
  uint4 Load4(uint address) const { return {Load(address), Load(address + 4), Load(address + 8), Load(address + 12)}; }
};

struct WordAccess {
  uint64_t reader, writer;
};

struct RWByteAddressBuffer : ByteAddressBuffer {
  uint *writable;
  WordAccess *access = nullptr;

  void note(size_t word, bool write) const {
    if (!access) return;
    const uint64_t lane = (uint64_t(gs_state.group_epoch) << 32) | gs_state.thread;
    auto &tag = access[word];
    if ((tag.writer && tag.writer != lane) || (write && tag.reader && tag.reader != lane)) {
      std::fprintf(stderr, "FAIL: %s has conflicting accesses to UAV word %zu\n", gs_state.shader, word);
      std::exit(1);
    }
    if (write) {
      tag.writer = lane;
    } else {
      tag.reader = tag.reader && tag.reader != lane ? ~uint64_t(0) : lane;
    }
  }

  uint Load(uint address) const {
    const uint value = ByteAddressBuffer::Load(address);
    note(address / 4, false);
    return value;
  }
  uint2 Load2(uint address) const { return {Load(address), Load(address + 4)}; }
  uint4 Load4(uint address) const { return {Load(address), Load(address + 4), Load(address + 8), Load(address + 12)}; }

  void Store(uint address, uint value) const {
    const size_t word = address / 4;
    check_access(word, address, count);
    note(word, true);
    writable[word] = value;
  }

  void Store2(uint address, uint2 v) const {
    Store(address, v.x);
    Store(address + 4, v.y);
  }

  void Store4(uint address, uint4 v) const {
    Store2(address, {v.x, v.y});
    Store2(address + 8, {v.z, v.w});
  }
};

inline void GroupMemoryBarrierWithGroupSync() { gs_group_sync(); }

template <class T>
struct StructuredBuffer {
  const T *entries;
  size_t count;

  const T &operator[](size_t i) const {
    if (i >= count) {
      std::fprintf(stderr, "FAIL: %s mode-info index %zu (count %zu)\n", gs_state.shader, i, count);
      std::exit(1);
    }
    return entries[i];
  }
};
