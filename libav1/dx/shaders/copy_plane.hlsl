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

  uint4 pixels = pool.Load4(cb_src_offset + x * 16 + y * cb_src_stride);
  pool.Store4(cb_dst_offset + x * 16 + y * cb_dst_stride, pixels);
}
