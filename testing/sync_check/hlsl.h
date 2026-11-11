/*
 * Enough HLSL for every compute shader of the decoder that has group memory to compile as C++ after
 * to_cpp.py has converted it: integer vectors with the swizzles the shaders use, the buffer types,
 * constant buffer arrays that can be indexed out of range (a GPU returns 0 there, random test input
 * does index out of range), and group memory that watches its readers and writers
 * (testing/common/group_sched.h). Integer maths wraps like on the GPU: build with -fwrapv.
 */
#ifndef SYNC_CHECK_HLSL_H_
#define SYNC_CHECK_HLSL_H_

#include <stdint.h>
#include <stdlib.h>
#include <string.h>

#include "../common/group_sched.h"

typedef unsigned int uint;

extern long g_out_of_bounds;  // buffer and constant buffer accesses outside the object

// ---------------------------------------------------------------------------
// Vectors
// ---------------------------------------------------------------------------

template <class T, int N>
struct vec;

// A swizzle: some components of a vector, in some order, as an lvalue. It lies over the vector.
template <class T, int N, int A, int B, int C = -1, int D = -1>
struct swizzle {
  T v[N];
  enum { kCount = 2 + (C >= 0) + (D >= 0) };
  operator vec<T, kCount>() const;
  swizzle &operator=(const vec<T, kCount> &o);
  swizzle &operator=(const swizzle &o) { return *this = (vec<T, kCount>)o; }
  template <int M, int A2, int B2, int C2, int D2>
  swizzle &operator=(const swizzle<T, M, A2, B2, C2, D2> &o) {
    return *this = (vec<T, kCount>)o;
  }
  swizzle &operator+=(const vec<T, kCount> &o);
};

template <class T>
struct vec<T, 2> {
  union {
    struct {
      T x, y;
    };
    swizzle<T, 2, 0, 1> xy;
    swizzle<T, 2, 1, 1> yy;
  };
  vec() : x(0), y(0) {}
  vec(T s) : x(s), y(s) {}
  vec(T a, T b) : x(a), y(b) {}
  vec(const vec &o) : x(o.x), y(o.y) {}
  vec &operator=(const vec &o) {
    x = o.x; y = o.y;
    return *this;
  }
  template <class U>
  vec(const vec<U, 2> &o) : x((T)o.x), y((T)o.y) {}
  T &operator[](int i) { return (&x)[i]; }
  const T &operator[](int i) const { return (&x)[i]; }
};

template <class T>
struct vec<T, 3> {
  union {
    struct {
      T x, y, z;
    };
    swizzle<T, 3, 0, 1> xy;
  };
  vec() : x(0), y(0), z(0) {}
  vec(T s) : x(s), y(s), z(s) {}
  vec(T a, T b, T c) : x(a), y(b), z(c) {}
  vec(const vec &o) : x(o.x), y(o.y), z(o.z) {}
  vec &operator=(const vec &o) {
    x = o.x; y = o.y; z = o.z;
    return *this;
  }
  template <class U>
  vec(const vec<U, 3> &o) : x((T)o.x), y((T)o.y), z((T)o.z) {}
  T &operator[](int i) { return (&x)[i]; }
  const T &operator[](int i) const { return (&x)[i]; }
};

template <class T>
struct vec<T, 4> {
  union {
    struct {
      T x, y, z, w;
    };
    swizzle<T, 4, 0, 1> xy;
    swizzle<T, 4, 2, 3> zw;
    swizzle<T, 4, 1, 2> yz;
    swizzle<T, 4, 0, 1, 2> xyz;
    swizzle<T, 4, 1, 2, 3> yzw;
    swizzle<T, 4, 0, 0, 0> xxx;
    swizzle<T, 4, 0, 1, 2, 3> xyzw;
    swizzle<T, 4, 3, 2, 1, 0> wzyx;
  };
  vec() : x(0), y(0), z(0), w(0) {}
  vec(T s) : x(s), y(s), z(s), w(s) {}
  vec(T a, T b, T c, T d) : x(a), y(b), z(c), w(d) {}
  vec(const vec &o) : x(o.x), y(o.y), z(o.z), w(o.w) {}
  vec &operator=(const vec &o) {
    x = o.x; y = o.y; z = o.z; w = o.w;
    return *this;
  }
  template <class U>
  vec(const vec<U, 4> &o) : x((T)o.x), y((T)o.y), z((T)o.z), w((T)o.w) {}
  T &operator[](int i) { return (&x)[i]; }
  const T &operator[](int i) const { return (&x)[i]; }
};

template <class T, int N, int A, int B, int C, int D>
swizzle<T, N, A, B, C, D>::operator vec<T, swizzle<T, N, A, B, C, D>::kCount>() const {
  const int at[4] = { A, B, C, D };
  vec<T, kCount> r;
  for (int i = 0; i < kCount; ++i) r[i] = v[at[i]];
  return r;
}
template <class T, int N, int A, int B, int C, int D>
swizzle<T, N, A, B, C, D> &swizzle<T, N, A, B, C, D>::operator=(const vec<T, kCount> &o) {
  const int at[4] = { A, B, C, D };
  const vec<T, kCount> copy = o;  // the source may lie over the destination
  for (int i = 0; i < kCount; ++i) v[at[i]] = copy[i];
  return *this;
}
template <class T, int N, int A, int B, int C, int D>
swizzle<T, N, A, B, C, D> &swizzle<T, N, A, B, C, D>::operator+=(const vec<T, kCount> &o) {
  const int at[4] = { A, B, C, D };
  for (int i = 0; i < kCount; ++i) v[at[i]] += o[i];
  return *this;
}

typedef vec<int, 2> int2;
typedef vec<int, 3> int3;
typedef vec<int, 4> int4;
typedef vec<uint, 2> uint2;
typedef vec<uint, 3> uint3;
typedef vec<uint, 4> uint4;

#define SYNC_CHECK_VEC_OP(op)                                           \
  template <class T, int N>                                             \
  vec<T, N> operator op(const vec<T, N> &a, const vec<T, N> &b) {       \
    vec<T, N> r;                                                        \
    for (int i = 0; i < N; ++i) r[i] = a[i] op b[i];                    \
    return r;                                                           \
  }                                                                     \
  template <class T, int N>                                             \
  vec<T, N> operator op(const vec<T, N> &a, T b) {                      \
    vec<T, N> r;                                                        \
    for (int i = 0; i < N; ++i) r[i] = a[i] op b;                       \
    return r;                                                           \
  }                                                                     \
  template <class T, int N>                                             \
  vec<T, N> operator op(T a, const vec<T, N> &b) {                      \
    vec<T, N> r;                                                        \
    for (int i = 0; i < N; ++i) r[i] = a op b[i];                       \
    return r;                                                           \
  }                                                                     \
  template <class T, int N>                                             \
  vec<T, N> &operator op##=(vec<T, N> &a, const vec<T, N> &b) {         \
    for (int i = 0; i < N; ++i) a[i] op## = b[i];                       \
    return a;                                                           \
  }                                                                     \
  template <class T, int N>                                             \
  vec<T, N> &operator op##=(vec<T, N> &a, T b) {                        \
    for (int i = 0; i < N; ++i) a[i] op## = b;                          \
    return a;                                                           \
  }
SYNC_CHECK_VEC_OP(+)
SYNC_CHECK_VEC_OP(-)
SYNC_CHECK_VEC_OP(*)
SYNC_CHECK_VEC_OP(&)
SYNC_CHECK_VEC_OP(|)
SYNC_CHECK_VEC_OP(<<)
SYNC_CHECK_VEC_OP(>>)
#undef SYNC_CHECK_VEC_OP

static inline int min(int a, int b) { return a < b ? a : b; }
static inline int max(int a, int b) { return a > b ? a : b; }
static inline uint min(uint a, uint b) { return a < b ? a : b; }
static inline uint max(uint a, uint b) { return a > b ? a : b; }
static inline int min(int a, uint b) { return min(a, (int)b); }
static inline int min(uint a, int b) { return min((int)a, b); }
static inline int max(int a, uint b) { return max(a, (int)b); }
static inline int max(uint a, int b) { return max((int)a, b); }
static inline int clamp(int v, int lo, int hi) { return min(max(v, lo), hi); }
template <class T, int N>
vec<T, N> clamp(const vec<T, N> &v, int lo, int hi) {
  vec<T, N> r;
  for (int i = 0; i < N; ++i) r[i] = (T)clamp((int)v[i], lo, hi);
  return r;
}

// Group memory of four component vectors: '.xy' of an element, as the shaders use it.
template <class T>
struct GsXyRef {
  vec<T, 4> *p;
  GsTag *tag;
  int line;
  operator vec<T, 2>() const {
    gs_note_read(tag, line);
    return vec<T, 2>(p->x, p->y);
  }
  const GsXyRef &operator=(const vec<T, 2> &v) const {
    gs_note_write(tag, line, p->x != v.x || p->y != v.y);
    p->x = v.x;
    p->y = v.y;
    return *this;
  }
  template <int N, int A, int B>
  const GsXyRef &operator=(const swizzle<T, N, A, B> &v) const {
    return *this = (vec<T, 2>)v;
  }
};
template <class T>
struct GsHlslVec4Ref : GsVec4Ref<vec<T, 4>> {
  GsXyRef<T> xy;
  GsHlslVec4Ref(vec<T, 4> *p_, GsTag *tag_, int line_) : GsVec4Ref<vec<T, 4>>(p_, tag_, line_), xy{ p_, tag_, line_ } {}
  GsHlslVec4Ref(const GsHlslVec4Ref &o) : GsHlslVec4Ref(o.p, o.tag, o.line) {}
  using GsVec4Ref<vec<T, 4>>::operator=;
};
template <class T>
struct GsRefOf<vec<T, 4>, 4> {
  typedef GsHlslVec4Ref<T> type;
};

// ---------------------------------------------------------------------------
// Constant buffer arrays
// ---------------------------------------------------------------------------

template <class T, size_t N>
struct CbArray {
  T data[N];
  const T &operator[](long i) const {
    static const T zero = T();
    if (i < 0 || (size_t)i >= N) {
      g_out_of_bounds++;
      return zero;
    }
    return data[i];
  }
};

// ---------------------------------------------------------------------------
// Buffers
// ---------------------------------------------------------------------------

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
  template <class T>
  void Store2(uint address, const vec<T, 2> &v) {
    Store(address, (uint)v.x);
    Store(address + 4, (uint)v.y);
  }
  template <class T, int N, int A, int B>
  void Store2(uint address, const swizzle<T, N, A, B> &v) {
    Store2(address, (vec<T, 2>)v);
  }
  template <class T>
  void Store4(uint address, const vec<T, 4> &v) {
    Store(address, (uint)v.x);
    Store(address + 4, (uint)v.y);
    Store(address + 8, (uint)v.z);
    Store(address + 12, (uint)v.w);
  }
};

template <class T>
struct StructuredBuffer {
  const T *ptr = nullptr;
  size_t count = 0;
  T operator[](long i) const {
    if (i < 0 || (size_t)i >= count) {
      g_out_of_bounds++;
      return T();
    }
    return ptr[i];
  }
};

template <class T>
struct RWStructuredBuffer {
  T *ptr = nullptr;
  size_t count = 0;
  T &operator[](long i) {
    static T spare;
    if (i < 0 || (size_t)i >= count) {
      g_out_of_bounds++;
      spare = T();
      return spare;
    }
    return ptr[i];
  }
};

// ---------------------------------------------------------------------------
// Barriers
// ---------------------------------------------------------------------------

static inline void GroupMemoryBarrier() { gs_state.plain_barriers++; }
static inline void GroupMemoryBarrierWithGroupSync() { gs_group_sync(); }
static inline void AllMemoryBarrierWithGroupSync() { gs_group_sync(); }

#endif  // SYNC_CHECK_HLSL_H_
