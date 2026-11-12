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

cbuffer ExtBorderSRT : register(b0) {
  uint4 cb_planes[3];
  uint4 cb_dims[2];
};

RWByteAddressBuffer Dst : register(u0);

[numthreads(64, 1, 1)] void main(uint3 thread
                                 : SV_DispatchThreadID) {
  const uint yh = cb_dims[0].y;
  const uint uvh = cb_dims[1].y;

  if (thread.x >= (yh + 2 * uvh)) return;

  const uint plane = (thread.x >= yh) + (thread.x >= (yh + uvh));
  const uint wi = thread.x - yh * (thread.x >= yh) - uvh * (thread.x >= (yh + uvh));

  const uint left_ptr = cb_planes[plane].y + wi * cb_planes[plane].x;
  const uint last_pix = (cb_dims[plane > 0].x << 1) - 2;
  const uint right_ptr = left_ptr + (last_pix & (~3));

  const uint shft = (last_pix & 3) * 8;
  uint left = (Dst.Load(left_ptr) & 0xffff) * 0x00010001;
  uint right0 = Dst.Load(right_ptr);
  uint right = ((right0 >> shft) & 0xffff) * 0x00010001;

  // The samples of the last word that belong to the frame stay, the rest becomes the last sample. The mask
  // of what stays comes from a table on purpose: FXC makes a bit field instruction of width shft out of
  // (1 << shft) - 1 and the line below, and a bit field of width 0 (an odd width) is one of the things GPUs
  // do not agree on.
  static const uint keep_mask[2] = {0x00000000, 0x0000ffff};
  uint r0mask = keep_mask[shft >> 4];
  right0 = (right0 & r0mask) | (right & (~r0mask));

  int i;
  for (i = -24; i < 0; i += 4) Dst.Store(left_ptr + i, left);

  Dst.Store(right_ptr, right0);
  for (i = 4; i <= 24; i += 4) Dst.Store(right_ptr + i, right);
}