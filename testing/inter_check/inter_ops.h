/*
 * The HLSL of testing/warp_check/hlsl_shim.h plus the vector arithmetic the inter prediction shaders
 * without a warp use (inter_main, inter_compound, inter_fast, inter_compound_fast): rows of four samples
 * times a tap, and sums of them. run.sh hands this file to every translation unit of inter_check.
 */
#ifndef INTER_CHECK_INTER_OPS_H_
#define INTER_CHECK_INTER_OPS_H_

#include "../warp_check/hlsl_shim.h"

static inline int4 operator*(const int4 &a, int b) { return int4(a.x * b, a.y * b, a.z * b, a.w * b); }
static inline int4 operator+(const int4 &a, const int4 &b) { return int4(a.x + b.x, a.y + b.y, a.z + b.z, a.w + b.w); }
static inline int4 &operator+=(int4 &a, const int4 &b) {
  a = a + b;
  return a;
}

#endif /* INTER_CHECK_INTER_OPS_H_ */
