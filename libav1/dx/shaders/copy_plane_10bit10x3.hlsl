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

RWByteAddressBuffer pool : register(u0);

cbuffer cb_sort_data : register(b0) {
  uint cb_wi_count;
  uint cb_src_offset;
  uint cb_src_stride;
  uint cb_dst_width;
  uint cb_dst_offset;
  uint cb_dst_stride;
};

[numthreads(64, 1, 1)] void main(uint3 thread
                                 : SV_DispatchThreadID) {
  if (thread.x >= cb_wi_count) return;

  const int x = thread.x % cb_dst_width;
  const int y = thread.x / cb_dst_width;

  int src_stride = cb_src_stride;
  int dst_stride = cb_dst_stride;
  int src_offset = cb_src_offset;
  int dst_offset = cb_dst_offset;
  uint base_src = y * src_stride + x * 48 + src_offset;
  uint base_trg = y * dst_stride + x * 32 + dst_offset;
  uint4 srcval1 = pool.Load4(base_src);
  uint4 srcval2 = pool.Load4(base_src + 16);
  uint4 srcval3 = pool.Load4(base_src + 32);
  uint4 trgval;

  trgval.x = (srcval1.x & 0x03FF) | ((srcval1.x & 0x03FF0000) >> 6) | ((srcval1.y & 0x03FF) << 20);
  trgval.y = ((srcval1.y & 0x03FF0000) >> 16) | ((srcval1.z & 0x03FF) << 10) | ((srcval1.z & 0x03FF0000) << 4);
  trgval.z = (srcval1.w & 0x03FF) | ((srcval1.w & 0x03FF0000) >> 6) | ((srcval2.x & 0x03FF) << 20);
  trgval.w = ((srcval2.x & 0x03FF0000) >> 16) | ((srcval2.y & 0x03FF) << 10) | ((srcval2.y & 0x03FF0000) << 4);
  pool.Store4(base_trg, trgval);

  trgval.x = (srcval2.z & 0x03FF) | ((srcval2.z & 0x03FF0000) >> 6) | ((srcval2.w & 0x03FF) << 20);
  trgval.y = ((srcval2.w & 0x03FF0000) >> 16) | ((srcval3.x & 0x03FF) << 10) | ((srcval3.x & 0x03FF0000) << 4);
  trgval.z = (srcval3.y & 0x03FF) | ((srcval3.y & 0x03FF0000) >> 6) | ((srcval3.z & 0x03FF) << 20);
  trgval.w = ((srcval3.z & 0x03FF0000) >> 16) | ((srcval3.w & 0x03FF) << 10) | ((srcval3.w & 0x03FF0000) << 4);
  pool.Store4(base_trg + 16, trgval);
}
