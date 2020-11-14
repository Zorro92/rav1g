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

// Per-vertex data used as input to the vertex shader.

struct PSInput {
  float4 position : SV_POSITION;
  float2 uv : TEXCOORD;
  //    float4 color : COLOR;
};

PSInput main(float4 position : POSITION, float2 uv : TEXCOORD) {
  PSInput result;

  result.position = position;
  result.uv = uv;

  return result;
}
