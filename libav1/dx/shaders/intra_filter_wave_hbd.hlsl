/*
 * Recursive filter intra shader (10-bit, AV1 7.11.2.3).
 *
 * Overlaps outer edge loads with residual preloads into LDS (res_table), eliminating
 * the 16-VGPR register shift queue (15 v_mov per diagonal) and cutting VGPRs from
 * 43 to 24 (12.5 waves/SIMD vs 10 waves/SIMD on RDNA2 wave64, zero s_barrier).
 */

cbuffer IntraDataCommon : register(b0) {
  int4 cb_planes[3];
  int4 cb_flags;
  int4 cb_filter[5][8][2];
  int4 cb_mode_params_lut[16][7];
  int4 cb_sm_weight_arrays[128];
};

cbuffer PSSLIntraSRT : register(b1) {
  uint cb_wi_count;
  int cb_pass_offset;
  int4 cb_counts0;
  int4 cb_counts1;
};

ByteAddressBuffer pred_blocks : register(t0);
ByteAddressBuffer residuals : register(t1);
ByteAddressBuffer wedge_mask : register(t2);
RWByteAddressBuffer dst_frame : register(u0);

#define UNIT_COLUMNS 8
groupshared uint unit_edges[3][2][UNIT_COLUMNS];
groupshared uint left_edges[16];
groupshared int res_table[16][32];

uint2 load_above(int base, int stride, int col, int above_available, int left_available) {
  uint2 pixels = uint2(0, 0x01ff0000);
  if (above_available)
    pixels = dst_frame.Load2(base - stride + min(above_available - 1, col) * 8);
  else if (left_available)
    pixels.y = dst_frame.Load(base - 4);
  if (col >= above_available) {
    const uint last = (pixels.y >> 16) & 1023;
    const uint pair = last | (last << 16);
    return uint2(pair, pair);
  }
  return uint2(pixels.x & 0x03ff03ff, pixels.y & 0x03ff03ff);
}

uint4 pack_taps(int4 lo, int4 hi) {
  return uint4((uint)lo.x + ((uint)hi.x << 16), (uint)lo.y + ((uint)hi.y << 16),
               (uint)lo.z + ((uint)hi.z << 16), (uint)lo.w + ((uint)hi.w << 16));
}

[numthreads(4, 8, 1)]
void main(uint3 thread : SV_GroupThreadID, uint3 gid : SV_GroupID) {
  if (gid.x * 8 >= cb_wi_count) return;

  const uint3 block = pred_blocks.Load3((cb_pass_offset + gid.x) << 4);
  const int columns = 1 << (block.y & 3);
  const int rows = 2 << (block.z & 3);
  const int plane = (block.y >> 3) & 3;
  const int mode = (block.z >> 4) & 7;
  const int4 plane_cb = plane == 0 ? cb_planes[0] : plane == 1 ? cb_planes[1] : cb_planes[2];
  const int stride = plane_cb.x;
  const int x = 8 * (block.x & 0xffff);
  const int y = 4 * (block.x >> 16);
  const int base = plane_cb.y + x + y * stride;
  const int above_available = (block.y >> 10) & 63;
  const int left_available = ((block.y >> 16) & 63) << 2;

  const int pair = thread.x;
  const int col = thread.y;
  const int lane = pair + 4 * col;

  // Issue above, left0, left1, and corner loads independently so latencies overlap.
  uint2 above_px = uint2(0, 0);
  uint left0 = 513, left1 = 513;
  uint corner0 = 512;

  if (lane < columns) {
    above_px = load_above(base, stride, lane, above_available, left_available);
  }
  if (lane < rows) {
    if (left_available) {
      left0 = dst_frame.Load(base - 4 + min(2 * lane, left_available - 1) * stride) >> 16;
      left1 = dst_frame.Load(base - 4 + min(2 * lane + 1, left_available - 1) * stride) >> 16;
    } else if (above_available) {
      left0 = dst_frame.Load(base - stride) & 1023;
      left1 = left0;
    }
    if (lane == 0) {
      if (above_available && left_available) corner0 = dst_frame.Load(base - stride - 4) >> 16;
    }
  }

  const bool add_residual = (block.y & (1 << 5)) != 0;
  const int res_stride = plane_cb.z;
  const int word_x = col * 8 + (pair & 1) * 4;
  const int word_y = pair >> 1;
  const int frame_offset = base + word_x + word_y * stride;
  const int res_offset = plane_cb.w + x + word_x + (y + word_y) * res_stride;

  if (add_residual && col < columns) {
    [loop] for (int q = 0; q < rows; ++q) {
      res_table[q][lane] = (int)residuals.Load(res_offset + 2 * q * res_stride);
    }
  }

  if (lane < columns) {
    unit_edges[1][lane & 1][lane] = above_px.x;
    unit_edges[2][lane & 1][lane] = above_px.y;
  }
  if (lane < rows) {
    left_edges[lane] = left0 | (left1 << 16);
    if (lane == 0) {
      if (!above_available || !left_available) {
        corner0 = (!above_available && !left_available) ? 512 : left0;
      }
      unit_edges[0][0][0] = corner0;
    }
  }

  const int tap = pair * 2;
  const uint4 a = pack_taps(cb_filter[mode][tap][0], cb_filter[mode][tap + 1][0]);
  const uint4 b = pack_taps(cb_filter[mode][tap][1], cb_filter[mode][tap + 1][1]);

  GroupMemoryBarrierWithGroupSync();

  uint corner = 0;
  if (col == 0) corner = unit_edges[0][0][0];
  else if (col < columns) corner = (uint)unit_edges[2][(col - 1) & 1][col - 1] >> 16;

  for (int diagonal = 0; diagonal < columns + rows - 1; ++diagonal) {
    if (diagonal != 0) GroupMemoryBarrierWithGroupSync();
    const int row = diagonal - col;
    if (col >= columns || row < 0 || row >= rows) continue;
    const int previous = diagonal & 1;

    const uint above01 = unit_edges[1][previous][col];
    const uint above23 = unit_edges[2][previous][col];
    uint left_a, left_b;
    if (col == 0) {
      const uint left = left_edges[row];
      left_a = left & 1023;
      left_b = left >> 16;
    } else {
      left_a = (uint)unit_edges[0][previous][col - 1] >> 16;
      left_b = (uint)unit_edges[2][previous][col - 1] >> 16;
    }

    const uint top_sum = (above01 & 1023) * a.y + (above01 >> 16) * a.z +
                         (above23 & 1023) * a.w + (above23 >> 16) * b.x;
    const uint left_sum = corner * a.x + left_a * b.y + left_b * b.z;
    const uint sum = top_sum + left_sum + 0x00080008u;
    int lo = clamp((int)(sum << 16) >> 20, 0, 1023);
    int hi = clamp((int)(sum + 0x8000u) >> 20, 0, 1023);

    if (pair != 0) unit_edges[pair - 1][previous ^ 1][col] = lo | (hi << 16);
    corner = left_b;

    if (add_residual) {
      const int residual = res_table[row][lane];
      lo = clamp(lo + ((int)((uint)residual << 16) >> 16), 0, 1023);
      hi = clamp(hi + (residual >> 16), 0, 1023);
    }
    dst_frame.Store(frame_offset + 2 * row * stride, lo | (hi << 16));
  }
}
