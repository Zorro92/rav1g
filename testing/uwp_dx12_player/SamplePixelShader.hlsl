/*
 * Copyright 2020 Google LLC
 *
 */

/*
 * Copyright (c) 2020, Alliance for Open Media. All rights reserved
 *
 * This source code is subject to the terms of the BSD 2 Clause License and
 * the Alliance for Open Media Patent License 1.0. If the BSD 2 Clause License
 * was not distributed with this source code in the LICENSE file, you can
 * obtain it at www.aomedia.org/license/software. If the Alliance for Open
 * Media Patent License 1.0 was not distributed with this source code in the
 * PATENTS file, you can obtain it at www.aomedia.org/license/patent.
 */

#include "ps_cbuf.h"

#define kDefaultWhiteLevelSRGB 100.0f  // sRGB
#define kDefaultWhiteLevelPQ \
  10000.0f  // PQ https://www.itu.int/dms_pubrec/itu-r/rec/bt/R-REC-BT.2100-0-201607-I!!PDF-E.pdf
#define kDefaultWhiteLevelHLG \
  1000.0  // HLG https://www.itu.int/dms_pubrec/itu-r/rec/bt/R-REC-BT.2100-0-201607-I!!PDF-E.pdf

// color conversion matrixs. It works with liner colors
static const float3x3 BT709_TO_BT2020 = {  // ref: ARIB STD-B62 and BT.2087
    0.6274, 0.3293, 0.0433, 0.0691, 0.9195, 0.0114, 0.0164, 0.0880, 0.8956};

static const float3x3 BT2020_TO_BT709 = {1.6605, -0.5877, -0.0728, -0.1246, 1.1330, -0.0084, -0.0182, -0.1006, 1.1187};

// YUV to RGB matrixs:
// BT.709
static const float3x3 YCbCR_TO_SRGB = {1.1644f, 0.0f, 1.7927f, 1.1644f, -0.2133f, -0.5329f, 1.1644f, 2.1124f, 0.0f};
// BT.2020
static const float3x3 YCbCr_TO_BT2020 = {1.163746465f, -0.028815145f, 2.823537589f, 1.164383561f, -0.258509894f,
                                         0.379693635f, 1.164383561f,  2.385315708f, 0.021554502f};

float3 SRGB_OETF(float3 L) {
  const float thr = 1.0;
  L = L / kDefaultWhiteLevelSRGB;
  float3 dark = L * 12.92;
  float3 light = 1.055 * (float3)pow(abs(L), 1.0 / 2.4) - 0.055;

  float3 r;
  bool3 cri = L <= 0.0031308;
  r = cri ? dark : light;

//#define SHOW_COLOR
#ifdef SHOW_COLOR
  float Y = 0.2126f * r.x + 0.7152f * r.y + 0.0722f * r.z;
  float3 ret = r;
  float gray = (3.0 - Y) / 4.0;
  float3 gray3 = float3(gray, gray, gray);
  if (Y > 3.0) {
    ret = float3(1.0, 0.0, 0.0);
  } else if (gray < 0.5) {
    ret = gray3;
  }
  return ret;
#else
  return r;
#endif
}

float3 SRGB_EOTF(float3 E) {
  float3 dark = E / 12.92;
  float3 light = pow(abs((E + 0.055) / (1 + 0.055)), 2.4);
  bool3 cri = E.xyz <= 0.04045;
  float3 r = cri ? dark : light;
  r = r * kDefaultWhiteLevelSRGB;
  return r;
}

float3 BT709_EOTF(float3 rgb) {
  const float a = 1.09929682f;
  const float b = 0.0180539685f;
  const float threshold = 0.081242864f;  // fromLiner(BT709, 0.0180539685f)
  bool3 cri = rgb < threshold;
  float3 dark = rgb / 4.5f;
  float3 light = pow(abs((rgb + a - 1.0f) / a), 1.0f / 0.45f);
  float3 liner709 = cri ? dark : light;

  return liner709 * kDefaultWhiteLevelSRGB;
}

// input: normalized L in units of RefWhite (1.0=100nits), output: normalized E
float3 PQ_OETF(float3 L) {
  const float c1 = 0.8359375;       // 3424.f/4096.f;
  const float c2 = 18.8515625;      // 2413.f/4096.f*32.f;
  const float c3 = 18.6875;         // 2392.f/4096.f*32.f;
  const float m1 = 0.159301758125;  // 2610.f / 4096.f / 4;
  const float m2 = 78.84375;        // 2523.f / 4096.f * 128.f;
  L = L / kDefaultWhiteLevelPQ;     // normalize liner color
  float3 Lm1 = pow(abs(L), m1);
  float3 X = (c1 + c2 * Lm1) / (1 + c3 * Lm1);
  float3 res = pow(abs(X), m2);
  return res;
}

// input: normalized E (0.0, 1.0), output: normalized L in units of RefWhite
float3 PQ_EOTF(float3 E) {
  const float c1 = 0.8359375;       // 3424.f/4096.f;
  const float c2 = 18.8515625;      // 2413.f/4096.f*32.f;
  const float c3 = 18.6875;         // 2392.f/4096.f*32.f;
  const float m1 = 0.159301758125;  // 2610.f / 4096.f / 4;
  const float m2 = 78.84375;        // 2523.f / 4096.f * 128.f;
  float3 M = c2 - c3 * pow(abs(E), 1 / m2);
  float3 N = max(pow(abs(E), 1 / m2) - c1, 0);
  float3 L = pow(abs(N / M), 1 / m1);  // normalized nits (1.0 = 10000nits)
  L = L * kDefaultWhiteLevelPQ;        // white level in range (0 - kDefaultWhiteLevelPQ);
  return L;
}

float3 ARIB_STD_B67_EOTF(float3 E) {
  const float a = 0.17883277f;
  const float b = 0.28466892f;
  const float c = 0.55991073f;
  const float Lmax = 12.0f;
  float3 dark = (E * 2.0f) * (E * 2.0f);
  float3 light = exp((E - c) / a) + b;
  bool3 cri = E <= 0.5f;
  float3 r = cri ? dark : light;
  return r / Lmax * kDefaultWhiteLevelHLG;
}
struct PSInput {
  float4 position : SV_POSITION;
  float2 uv : TEXCOORD;
};

Texture2D g_textureY : register(t0);
Texture2D g_textureU : register(t1);
Texture2D g_textureV : register(t2);
SamplerState g_samplerY : register(s0);
SamplerState g_samplerUV : register(s1);

float4 main(PSInput fragment) : SV_TARGET {
  float3 ycbcr;
  if (hdr_10_10_10_2) {
    uint2 DTid, ptex_dim, ptex_ind, ptex_ind_temp;
    float2 ptex_pos, tex_pos, mag_ratio;

    mag_ratio.x = float(disp_w) / float(frame_w);
    mag_ratio.y = float(disp_h) / float(frame_h);
    DTid.x = uint(floor(fragment.uv.x * disp_w));
    DTid.y = uint(floor(fragment.uv.y * disp_h));
    tex_pos = (float2(DTid) + 0.5f) / mag_ratio;

    ptex_dim = uint2(ptex_w, ptex_h);
    ptex_ind = uint2(floor(tex_pos.x / 3), floor(tex_pos.y));
    ptex_pos.x = ((float)ptex_ind + 0.5f) / (float)(ptex_dim.x);
    ptex_pos.y = tex_pos.y / (float)(ptex_dim.y);
    ycbcr.x = g_textureY.Sample(g_samplerY, ptex_pos)[uint(floor(tex_pos.x)) % 3];

    ptex_dim >>= 1;
    DTid >>= 1;
    ptex_ind_temp = uint2(floor((float(DTid.x + 0.5f) / mag_ratio.x) / 3), floor(float(DTid.y + 0.5f) / mag_ratio.y));
    ptex_ind = ptex_ind_temp;  // >> 1;
    ptex_pos = ((float2)ptex_ind + 0.5f) / (float2)(ptex_dim);

    ycbcr.y = g_textureU.Sample(g_samplerUV, ptex_pos)[uint(floor(float(DTid.x + 0.5f) / mag_ratio.x)) % 3];
    ycbcr.z = g_textureV.Sample(g_samplerUV, ptex_pos)[uint(floor(float(DTid.x + 0.5f) / mag_ratio.x)) % 3];
  } else if (isMonochrome) {
    float2 cbcr = isMonochrome ? float2(0.5, 0.5)
                               : float2(g_textureU.Sample(g_samplerUV, fragment.uv).x,
                                        g_textureV.Sample(g_samplerUV, fragment.uv).x);
    ycbcr = float3(g_textureY.Sample(g_samplerY, fragment.uv).x, cbcr);

  } else {
    ycbcr = float3(g_textureY.Sample(g_samplerY, fragment.uv).x, g_textureU.Sample(g_samplerUV, fragment.uv).x,
                   g_textureV.Sample(g_samplerUV, fragment.uv).x);
  }
  ycbcr *= scale_factor;
  ycbcr -= float3(0.0625, 0.5, 0.5);
  float4 result = float4(mul(YCbCR_TO_SRGB, ycbcr), 1.0f);
  return result;
}