/*
 * SPDX-License-Identifier: BSD-2-Clause
 *
 * This source code is subject to the terms of the BSD 2 Clause License and the Alliance for Open Media
 * Patent License 1.0. If the BSD 2 Clause License was not distributed with this source code in the LICENSE
 * file, you can obtain it at www.aomedia.org/license/software. If the Alliance for Open Media Patent
 * License 1.0 was not distributed with this source code in the PATENTS file, you can obtain it at
 * www.aomedia.org/license/patent.
 */

#include "film_grain_const.h"
#include "film_grain_apply.h"

cbuffer cb_film_grain_data : register(b0) { FilmGrainData data; };

ByteAddressBuffer src : register(t0);
ByteAddressBuffer grain_block : register(t1);
StructuredBuffer<int> random_offset : register(t2);

#define SP_TEXTURE
#define SP_LUMA_REGISTER u0
#define SP_CHROMA_REGISTER u1
#include "output_sp.h"

struct GrainTile {
  int current;
  int left;
  int above;
  int above_left;
  bool overlap_x;
  bool overlap_y;
};

// The random offset selects a patch of the plane's grain template. For overlap, local also
// includes a block width and/or height to pick up the previous patch's right/bottom edge.
int template_address(int offset, int2 local, int stride, int subsampling, int base) {
  const int2 origin = int2(data.left_pad, data.top_pad) + ((2 * data.ar_padding) >> subsampling);
  const int2 shift = int2(offset >> 4, offset & 15) << (1 - subsampling);
  const int2 sample_pos = origin + shift + local;
  return 4 * (base + sample_pos.y * stride + sample_pos.x);
}

int2 blend_luma_x(int2 left, int2 current, int center) {
  return clamp((left * int2(27, 17) + current * int2(17, 27) + 16) >> 5, -center, center - 1);
}

int4 luma_grain(GrainTile tile, int2 local, int center) {
  const int stride = data.luma_grain_stride;
  const int2 dx = int2(luma_subblock_size_x, 0);
  const int2 dy = int2(0, luma_subblock_size_y);
  int4 current = (int4)grain_block.Load4(template_address(tile.current, local, stride, 0, 0));
  if (tile.overlap_x) {
    const int2 left = (int2)grain_block.Load2(template_address(tile.left, local + dx, stride, 0, 0));
    current.xy = blend_luma_x(left, current.xy, center);
  }
  if (tile.overlap_y) {
    int4 above = (int4)grain_block.Load4(template_address(tile.above, local + dy, stride, 0, 0));
    if (tile.overlap_x) {
      const int2 above_left =
          (int2)grain_block.Load2(template_address(tile.above_left, local + dx + dy, stride, 0, 0));
      above.xy = blend_luma_x(above_left, above.xy, center);
    }
    // Both horizontal blends are rounded and clipped before blending the rows.
    const int w = 27 - 10 * local.y;
    current = clamp((above * w + current * (44 - w) + 16) >> 5, -center, center - 1);
  }
  return current;
}

int blend_chroma(int previous, int current, int center) {
  return clamp((previous * 23 + current * 22 + 16) >> 5, -center, center - 1);
}

int2 chroma_grain(GrainTile tile, int2 local, int center, int base) {
  const int stride = data.chroma_grain_stride;
  const int2 dx = int2(chroma_subblock_size_x, 0);
  const int2 dy = int2(0, chroma_subblock_size_y);
  int2 current = (int2)grain_block.Load2(template_address(tile.current, local, stride, 1, base));
  if (tile.overlap_x) {
    const int left = (int)grain_block.Load(template_address(tile.left, local + dx, stride, 1, base));
    current.x = blend_chroma(left, current.x, center);
  }
  if (tile.overlap_y) {
    int2 above = (int2)grain_block.Load2(template_address(tile.above, local + dy, stride, 1, base));
    if (tile.overlap_x) {
      const int above_left = (int)grain_block.Load(template_address(tile.above_left, local + dx + dy, stride, 1, base));
      above.x = blend_chroma(above_left, above.x, center);
    }
    current.x = blend_chroma(above.x, current.x, center);
    current.y = blend_chroma(above.y, current.y, center);
  }
  return current;
}

int scaling(int index, int plane, int bit_shift) {
  const int x = index >> bit_shift;
  const int scale = data.scaling_lut[x][plane];
  if (bit_shift == 0 || x == 255) return scale;
  const int next = data.scaling_lut[x + 1][plane];
  return scale + (((next - scale) * (index & 3) + 2) >> 2);
}

int4 load_luma(int2 pixel, bool hbd) {
  const int2 plane = data.src_planes[0].xy;
  const int row = plane.y + pixel.y * plane.x;
  if (hbd) {
    const uint2 value = src.Load2(row + pixel.x * 2);
    return int4(value.x & 1023u, (value.x >> 16) & 1023u, value.y & 1023u, (value.y >> 16) & 1023u);
  }
  const uint value = src.Load(row + pixel.x);
  return int4(value & 255u, (value >> 8) & 255u, (value >> 16) & 255u, value >> 24);
}

int2 load_chroma(int2 pixel, int plane, bool hbd) {
  const int2 layout = data.src_planes[plane].xy;
  const int row = layout.y + pixel.y * layout.x;
  if (hbd) {
    const uint value = src.Load(row + pixel.x * 2);
    return int2(value & 1023u, (value >> 16) & 1023u);
  }
  const uint value = src.Load(row + (pixel.x & ~3)) >> ((pixel.x & 2) * 8);
  return int2(value & 255u, (value >> 8) & 255u);
}

int2 apply_chroma(GrainTile tile, int2 local, int2 pixel, int2 mean_luma, int plane, int bit_shift, int center) {
  const int2 input = load_chroma(pixel, plane, bit_shift != 0);
  const bool from_luma = data.params.chroma_scaling_from_luma != 0;
  const int points = plane == 1 ? data.params.num_cb_points : data.params.num_cr_points;
  if (!from_luma && points == 0) return input;

  const int grain_base = plane == 1 ? data.grain_offset_u : data.grain_offset_v;
  const int sample_max = 2 * center - 1;
  int2 index = mean_luma;
  if (!from_luma) {
    const int mult = (plane == 1 ? data.params.cb_mult : data.params.cr_mult) - 128;
    const int luma_mult = (plane == 1 ? data.params.cb_luma_mult : data.params.cr_luma_mult) - 128;
    const int offset = ((plane == 1 ? data.params.cb_offset : data.params.cr_offset) << bit_shift) - 2 * center;
    index = clamp(((mean_luma * luma_mult + input * mult) >> 6) + offset, 0, sample_max);
  }
  const int2 scale = int2(scaling(index.x, plane, bit_shift), scaling(index.y, plane, bit_shift));
  const int2 grain = chroma_grain(tile, local, center, grain_base);
  const int rounding = 1 << (data.params.scaling_shift - 1);
  const int lo = data.params.clip_to_restricted_range ? min_chroma_legal_range << bit_shift : 0;
  const int hi = data.params.clip_to_restricted_range
                     ? (data.mc_identity ? max_luma_legal_range : max_chroma_legal_range) << bit_shift
                     : sample_max;
  return clamp(input + ((scale * grain + rounding) >> data.params.scaling_shift), lo, hi);
}

// One 32x32 grain block per group. Each lane owns four luma samples and, on even rows of a
// 4:2:0 frame with chroma enabled, the two chroma pairs that use those ungrained luma samples.
[numthreads(8, 32, 1)]
void main(int3 group : SV_GroupID, int3 lane : SV_GroupThreadID) {
  const int2 local = int2(lane.x * 4, lane.y);
  const int2 pixel = int2(group.x * luma_subblock_size_x, group.y * luma_subblock_size_y) + local;
  if (pixel.x >= data.width || pixel.y >= data.height) return;

  const int block = group.y * data.random_offset_stride + group.x;
  GrainTile tile;
  tile.overlap_x = data.params.overlap_flag && group.x != 0 && local.x == 0;
  tile.overlap_y = data.params.overlap_flag && group.y != 0 && local.y < 2;
  tile.current = random_offset[block];
  tile.left = 0;
  tile.above = 0;
  tile.above_left = 0;
  if (tile.overlap_x) tile.left = random_offset[block - 1];
  if (tile.overlap_y) {
    const int above = block - data.random_offset_stride;
    tile.above = random_offset[above];
    if (tile.overlap_x) tile.above_left = random_offset[above - 1];
  }

  const int bit_shift = int(data.params.bit_depth) - 8;
  const int center = 128 << bit_shift;
  const int4 input = load_luma(pixel, bit_shift != 0);
  int4 output = input;
  if (data.params.num_y_points != 0) {
    const int4 grain = luma_grain(tile, local, center);
    const int4 scale = int4(scaling(input.x, 0, bit_shift), scaling(input.y, 0, bit_shift),
                            scaling(input.z, 0, bit_shift), scaling(input.w, 0, bit_shift));
    const int rounding = 1 << (data.params.scaling_shift - 1);
    const int lo = data.params.clip_to_restricted_range ? min_luma_legal_range << bit_shift : 0;
    const int hi = data.params.clip_to_restricted_range ? max_luma_legal_range << bit_shift : 2 * center - 1;
    output = clamp(input + ((scale * grain + rounding) >> data.params.scaling_shift), lo, hi);
  }
  sp_store_luma(pixel.x, pixel.y, output, data.width, data.height, bit_shift != 0, data.dst_planes[0].xy);

  if (data.enable_chroma && (local.y & 1) == 0) {
    const int2 uv = pixel >> 1;
    const int2 mean_luma = int2(input.x + input.y + 1, input.z + input.w + 1) >> 1;
    const int2 cb = apply_chroma(tile, local >> 1, uv, mean_luma, 1, bit_shift, center);
    const int2 cr = apply_chroma(tile, local >> 1, uv, mean_luma, 2, bit_shift, center);
    const int shift = bit_shift != 0 ? 6 : 0;
    sp_chroma[uv] = uint2(cb.x, cr.x) << shift;
    sp_chroma[uv + int2(1, 0)] = uint2(cb.y, cr.y) << shift;
  }
}
