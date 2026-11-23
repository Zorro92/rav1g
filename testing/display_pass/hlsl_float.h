/*
 * SPDX-License-Identifier: BSD-2-Clause
 *
 * The float side of HLSL that display_pass.hlsl uses, on top of testing/sync_check/hlsl.h: float
 * vectors, the vector operators hlsl.h has no use for (division, vector min/max/clamp), and the float
 * intrinsics. The transcendental functions are the C library's (powf, expf, log2f): not the GPU's, which
 * is why the check holds the result to the model with a tolerance (model.py) and not bit for bit.
 */
#ifndef DISPLAY_PASS_HLSL_FLOAT_H_
#define DISPLAY_PASS_HLSL_FLOAT_H_

#include <math.h>
#include <string.h>

#include "hlsl.h"

typedef vec<float, 2> float2;
typedef vec<float, 3> float3;
typedef vec<float, 4> float4;

template <class T, int N>
vec<T, N> operator/(const vec<T, N> &a, const vec<T, N> &b) {
  vec<T, N> r;
  for (int i = 0; i < N; ++i) r[i] = a[i] / b[i];
  return r;
}
template <class T, int N>
vec<T, N> operator/(const vec<T, N> &a, T b) {
  vec<T, N> r;
  for (int i = 0; i < N; ++i) r[i] = a[i] / b;
  return r;
}
template <class T, int N>
vec<T, N> operator<<(const vec<T, N> &v, int shift) {
  return v << (T)shift;
}

template <class T, int N>
vec<T, N> min(const vec<T, N> &a, const vec<T, N> &b) {
  vec<T, N> r;
  for (int i = 0; i < N; ++i) r[i] = a[i] < b[i] ? a[i] : b[i];
  return r;
}
template <class T, int N>
vec<T, N> max(const vec<T, N> &a, const vec<T, N> &b) {
  vec<T, N> r;
  for (int i = 0; i < N; ++i) r[i] = a[i] > b[i] ? a[i] : b[i];
  return r;
}
template <class T, int N>
vec<T, N> clamp(const vec<T, N> &v, const vec<T, N> &lo, const vec<T, N> &hi) {
  return min(max(v, lo), hi);
}

static inline float min(float a, float b) { return fminf(a, b); }
static inline float max(float a, float b) { return fmaxf(a, b); }
static inline float clamp(float v, float lo, float hi) { return fminf(fmaxf(v, lo), hi); }
static inline float frac(float v) { return v - floorf(v); }
static inline float saturate(float v) { return clamp(v, 0.0f, 1.0f); }
static inline float3 saturate(const float3 &v) { return float3(saturate(v.x), saturate(v.y), saturate(v.z)); }
// abs, pow, exp, log2, floor and ceil of a float: the float overloads of <math.h>.
// The GPU's mad may round once or twice; the check does not depend on which.
static inline float mad(float a, float b, float c) { return a * b + c; }
template <int N>
vec<float, N> floor(const vec<float, N> &v) {
  vec<float, N> r;
  for (int i = 0; i < N; ++i) r[i] = floorf(v[i]);
  return r;
}
template <int N>
vec<float, N> ceil(const vec<float, N> &v) {
  vec<float, N> r;
  for (int i = 0; i < N; ++i) r[i] = ceilf(v[i]);
  return r;
}
static inline float dot(const float3 &a, const float3 &b) { return a.x * b.x + a.y * b.y + a.z * b.z; }
static inline uint asuint(float v) {
  uint u;
  memcpy(&u, &v, 4);
  return u;
}
static inline float asfloat(uint u) {
  float v;
  memcpy(&v, &u, 4);
  return v;
}

// Group memory of float3: watched as a whole, all three components compared.
struct GsFloat3Ref {
  float3 *p;
  GsTag *tag;
  int line;
  GsFloat3Ref(float3 *p_, GsTag *tag_, int line_) : p(p_), tag(tag_), line(line_) {}
  operator float3() const {
    gs_note_read(tag, line);
    return *p;
  }
  const GsFloat3Ref &operator=(const float3 &v) const {
    gs_note_write(tag, line, memcmp(p, &v, sizeof(v)) != 0);
    *p = v;
    return *this;
  }
};
template <>
struct GsRefOf<float3, 2> {
  typedef GsFloat3Ref type;
};

// A typed texture of the output, as copy_check has it: stores outside are discarded, every store counted.
template <class T>
struct Texture {
  uint width = 0, height = 0;
  T *samples = nullptr;
  uint *writes = nullptr;
  T discarded{};
  T &store(const int2 &pos) {
    if (pos.x < 0 || pos.y < 0 || uint(pos.x) >= width || uint(pos.y) >= height) return discarded;
    const uint i = uint(pos.y) * width + uint(pos.x);
    ++writes[i];
    return samples[i];
  }
};
template <class T>
struct RWTexture2D {
  Texture<T> *texture;
  T &operator[](const int2 &pos) const { return texture->store(pos); }
};

#endif  // DISPLAY_PASS_HLSL_FLOAT_H_
