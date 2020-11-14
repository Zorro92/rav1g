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

#define Types 20
#define TypeMask 0xff
#define TypeSize 8

cbuffer cb_sort_data : register(b0) {
  uint cb_count;
  uint cb_src_offset;
  int2 reserved;
  int4 cb_offsets[Types];
};

ByteAddressBuffer src : register(t0);
RWByteAddressBuffer dst : register(u0);

[numthreads(64, 1, 1)] void main(uint3 thread
                                 : SV_DispatchThreadID) {
  if (thread.x >= cb_count) return;
  int4 block = src.Load4((thread.x + cb_src_offset) * 16);
  uint type = block.w & TypeMask;
  uint offset = block.w >> TypeSize;
  if (type >= Types) return;

  int index = cb_offsets[type].x + offset;
  dst.Store4(index * 16, block);
}
