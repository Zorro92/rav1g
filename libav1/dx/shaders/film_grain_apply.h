/*
 * SPDX-License-Identifier: BSD-2-Clause
 *
 * This source code is subject to the terms of the BSD 2 Clause License and the Alliance for Open Media
 * Patent License 1.0. If the BSD 2 Clause License was not distributed with this source code in the LICENSE
 * file, you can obtain it at www.aomedia.org/license/software. If the Alliance for Open Media Patent
 * License 1.0 was not distributed with this source code in the PATENTS file, you can obtain it at
 * www.aomedia.org/license/patent.
 */

// Layout shared by the apply shaders and FilmGrainData in the Rust host.
struct FilmGrainData {
  GrainParams params;

  int4 src_planes[3];
  int4 dst_planes[3];

  int enable_chroma;
  int random_offset_stride;
  int width;
  int height;

  int mc_identity;
  int luma_grain_stride;
  int chroma_grain_stride;
  int left_pad;

  int right_pad;
  int top_pad;
  int bottom_pad;
  int ar_padding;

  int grain_offset_u;
  int grain_offset_v;
  int is_10x3;
  int pad;

  int4 scaling_lut[256];
};

// Chroma uses pairs of ungrained luma samples (AV1 7.18.3.5). At the right edge
// of an odd-width frame the last sample supplies both values of its pair.
int2 chroma_luma_average(int4 luma, int x, int width) {
  const int remaining = width - x;
  const int2 next = int2(remaining == 1 ? luma.x : luma.y, remaining == 3 ? luma.z : luma.w);
  return (int2(luma.x, luma.z) + next + 1) >> 1;
}
