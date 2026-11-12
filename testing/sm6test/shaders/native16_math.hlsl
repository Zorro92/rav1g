// Shader Model 6 test (docs/test-player.md, "Shader Model 6 test"): native 16-bit types (cs_6_2,
// -enable-16bit-types). uint16_t, int16_t and float16_t arithmetic of the kind the decoder's shaders do on
// samples: packed 8-bit and 10-bit samples are unpacked into 16-bit vectors, averaged, weighted, shifted,
// clamped, and packed again. Some results overflow 16 bits on purpose, and some of those are shifted right
// afterwards: they are right only where the arithmetic really wraps at 16 bits.
//
// Thread i of 256 reads four words: two words of four 8-bit samples (P, Q) and two words of three 10-bit
// samples (X, Y), and writes a record of 17 words. The host computes the same record with uint16_t and
// int16_t (testing/sm6test/sm6test.cpp, CheckNative16Math).

#define REC 17

StructuredBuffer<uint> src : register(t0);
RWStructuredBuffer<uint> dst : register(u0);

uint16_t4 Unpack8(uint w) { return (uint16_t4)uint4(w & 0xff, (w >> 8) & 0xff, (w >> 16) & 0xff, w >> 24); }

uint Pack8(uint16_t4 v) {
  const uint4 u = (uint4)v & 0xff;
  return u.x | (u.y << 8) | (u.z << 16) | (u.w << 24);
}

uint16_t3 Unpack10(uint w) { return (uint16_t3)uint3(w & 0x3ff, (w >> 10) & 0x3ff, (w >> 20) & 0x3ff); }

uint Pack10(uint16_t3 v) {
  const uint3 u = (uint3)v & 0x3ff;
  return u.x | (u.y << 10) | (u.z << 20);
}

[numthreads(64, 1, 1)]
void main(uint3 tid : SV_DispatchThreadID) {
  const uint id = tid.x;
  const uint16_t4 P = Unpack8(src[id * 4]);
  const uint16_t4 Q = Unpack8(src[id * 4 + 1]);
  const uint16_t3 X = Unpack10(src[id * 4 + 2]);
  const uint16_t3 Y = Unpack10(src[id * 4 + 3]);
  const uint o = id * REC;

  // 8-bit samples.
  dst[o + 0] = Pack8((P + Q + 1) >> 1);
  const uint16_t sum_p = P.x + P.y + P.z + P.w;
  const uint16_t sum_q = Q.x + Q.y + Q.z + Q.w;
  dst[o + 1] = (uint)sum_p | ((uint)sum_q << 16);
  dst[o + 2] = Pack8((P * 3 + Q * 13 + 8) >> 4);
  const int16_t4 D = (int16_t4)P - (int16_t4)Q;
  dst[o + 3] = Pack8((uint16_t4)(D >> 2));  // arithmetic shift of a negative difference
  dst[o + 4] = Pack8((uint16_t4)clamp((int16_t4)P + D * 4, (int16_t)0, (int16_t)255));
  const uint16_t4 W = P * Q * 5;  // up to 325125: wraps at 16 bits
  dst[o + 5] = (uint)W.x | ((uint)W.y << 16);
  dst[o + 6] = (uint)(uint16_t)(W.z >> 3) | ((uint)(uint16_t)(W.w >> 3) << 16);  // a shift of the wrapped value
  const uint16_t u = (P.x << 8) | Q.x;
  const int16_t s = (int16_t)u;  // bit 15 is the sign
  dst[o + 7] = (uint)(int)(s >> 4);  // sign extension to 32 bits
  // Unsigned, so that the overflow is defined: wraps, and then an arithmetic shift of the 16-bit value.
  dst[o + 8] = (uint)(uint16_t)((int16_t)(uint16_t)(u * 3) >> 4);

  // 10-bit samples.
  dst[o + 9] = Pack10((X + Y + 1) >> 1);
  dst[o + 10] = Pack10((X * 9 + Y * 7 + 8) >> 4);
  const int16_t3 E = (int16_t3)X - (int16_t3)Y;
  dst[o + 11] = Pack10((uint16_t3)clamp((int16_t3)X + ((E * 3) >> 1), (int16_t)0, (int16_t)1023));
  const uint16_t3 M = X * Y;  // up to 1046529: wraps at 16 bits
  dst[o + 12] = (uint)M.x | ((uint)(uint16_t)(M.y >> 2) << 16);
  const uint16_t3 A = (uint16_t3)abs(E);
  dst[o + 13] = (uint)(uint16_t)(A.x + A.y + A.z) | ((uint)M.z << 16);

  // float16_t: a weighted sum whose every step is exact in 16-bit floating point.
  const float16_t f = (float16_t)P.x * (float16_t)0.25 + (float16_t)Q.x * (float16_t)0.75;
  dst[o + 14] = (uint)(f + (float16_t)0.5) | ((uint)(f * (float16_t)4.0) << 16);
  // 2048..3071 into a float16_t: 16-bit floating point has even numbers only there. Reported, not judged:
  // says whether a float16_t is a 16-bit float on this device or a wider one.
  const float16_t h = (float16_t)(uint16_t)(X.x + 2048);
  dst[o + 15] = (uint)h;
  dst[o + 16] = 0x16B17000 | (sizeof(uint16_t) << 8) | (sizeof(int16_t) << 4) | sizeof(float16_t);
}
