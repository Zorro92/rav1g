/*
 * Fast wave64 intra prediction shader (10-bit; 8-bit when INTRA_8BIT is defined).
 *
 * Selected per wavefront step when every main-shader block in the step uses only
 * DC, straight V/H, or Smooth/SmoothV/SmoothH (no directional, Paeth, IntraBC,
 * CfL, or InterIntra). AV1 Spec 7.11.2.1, 7.11.2.2, 7.11.2.5, 7.11.2.7.
 *
 * Sized for the console's wave64 compute units (AGENTS.md):
 * - WG_SIZE = 64 (1 wave64/group): zero s_barrier on RDNA2, 512 B LDS, <=40 VGPRs
 *   (12 waves/SIMD vs 8 waves/SIMD for the full 256-thread shader).
 * - Large blocks (>16x16) pack 2 or 4 rows per lane so 32x32 fits in 1 wave64
 *   (64 lanes), 32x64/64x32 in 2 waves (128 lanes), and 64x64 in 4 waves (256 lanes).
 * - V/H/Smooth read edges directly without LDS; >=16x16 DC uses WaveActiveSum
 *   with zero LDS traffic.
 */

cbuffer IntraDataCommon : register(b0) {
  int4 cb_planes[3];
  int4 cb_flags;
  int4 cb_filter[5][8][2];
  int4 cb_mode_params_lut[16][7];
  int4 cb_sm_weight_arrays[128];
};

cbuffer PSSLIntraSRT : register(b1) {
  int4 cb_counts0;
  int4 cb_counts1;
  uint cb_wi_count;
  int cb_pass_offset;
};

ByteAddressBuffer pred_blocks : register(t0);
ByteAddressBuffer residuals : register(t1);
ByteAddressBuffer wedge_mask : register(t2);
RWByteAddressBuffer dst_frame : register(u0);

#define WG_SIZE 64
#define MODE_V 0
#define MODE_H 1
#define MODE_SMOOTH 8
#define MODE_SMOOTH_V 9
#define MODE_SMOOTH_H 10
#define MODE_DC 14

#ifdef INTRA_8BIT
#define PIXEL_MAX 255
#define PIXEL_MID 128
#define LEFT_SHIFT 24
#else
#define PIXEL_MAX 1023
#define PIXEL_MID 512
#define LEFT_SHIFT 16
#endif

// Small scratchpad for DC reduction on sub-wave64 blocks (4x4..8x16) or small-wave devices.
groupshared int dc_scratch[WG_SIZE];

#ifdef INTRA_8BIT
uint load_above_pair(int corner, int wi, int available) {
  const int at = min(wi, available - 1);
  const uint word = dst_frame.Load(corner + 4 + 4 * (at >> 1));
  const uint half_word = (at & 1) ? word >> 16 : word & 0xffff;
  uint pair = (half_word & 255) | ((half_word >> 8) << 16);
  if (wi >= available) pair = (pair & 0xffff0000) | (pair >> 16);
  return pair;
}

uint pack_samples(uint2 pixels) {
  return (pixels.x & 255) | (((pixels.x >> 16) & 255) << 8) | ((pixels.y & 255) << 16) |
         (((pixels.y >> 16) & 255) << 24);
}
#else
uint load_above_pair(int corner, int wi, int available) {
  uint pair = dst_frame.Load(corner + 4 + 4 * min(wi, available - 1));
  if (wi >= available) pair = (pair & 0xffff0000) | (pair >> 16);
  return pair;
}
#endif

int load_left(int corner, int stride, int wi, int available) {
  return dst_frame.Load(corner + (min(wi, available - 1) + 1) * stride) >> LEFT_SHIFT;
}

uint fast_dc_div(uint sum, uint count) {
  // count is bw, bh, or bw + bh with bw, bh in {4,8,16,32,64} and max/min <= 4.
  // count >> firstbitlow(count) is therefore always 1, 3, or 5.
  const uint shift = firstbitlow(count);
  const uint odd = count >> shift;
  uint s = (sum + (count >> 1)) >> shift;
  if (odd != 1) {
    const uint mult = (odd == 3) ? 21846u : 13108u;
    s = (s * mult) >> 16;
  }
  return s;
}

uint2 add_res_and_pack(uint2 px, int2 res) {
  px.x = clamp((int)(px.x & 1023) + (int)((res.x << 16) >> 16), 0, PIXEL_MAX) |
         (clamp((int)((px.x >> 16) & 1023) + (int)(res.x >> 16), 0, PIXEL_MAX) << 16);
  px.y = clamp((int)(px.y & 1023) + (int)((res.y << 16) >> 16), 0, PIXEL_MAX) |
         (clamp((int)((px.y >> 16) & 1023) + (int)(res.y >> 16), 0, PIXEL_MAX) << 16);
  return px;
}

[numthreads(WG_SIZE, 1, 1)]
void main(uint3 thread : SV_DispatchThreadID) {
  const int4 counts0 = cb_counts0;
  const int4 counts1 = cb_counts1;
  int area_log = 10;
  int lanes_log = 8;
  int offset = cb_pass_offset;
  int threadx = (int)thread.x;
  uint3 block = uint3(0, 0, 0);
  int block_lane = 0;
  int wi_count = 0;
  int wi = WG_SIZE;

  const bool active = thread.x < cb_wi_count;
  if (active) {
    if (threadx >= (counts0.x << 8)) {
      offset += counts0.x;
      threadx -= counts0.x << 8;
      area_log = 9;
      lanes_log = 7;
      if (threadx >= (counts0.y << 7)) {
        offset += counts0.y;
        threadx -= counts0.y << 7;
        area_log = 8;
        lanes_log = 6;
        if (threadx >= (counts0.z << 6)) {
          offset += counts0.z;
          threadx -= counts0.z << 6;
          area_log = 7;
          if (threadx >= (counts0.w << 6)) {
            offset += counts0.w;
            threadx -= counts0.w << 6;
            area_log = 6;
            if (threadx >= (counts1.x << 6)) {
              offset += counts1.x;
              threadx -= counts1.x << 6;
              area_log = 5;
              lanes_log = 5;
              if (threadx >= (counts1.y << 5)) {
                offset += counts1.y;
                threadx -= counts1.y << 5;
                area_log = 4;
                lanes_log = 4;
                if (threadx >= (counts1.z << 4)) {
                  offset += counts1.z;
                  threadx -= counts1.z << 4;
                  area_log = 3;
                  lanes_log = 3;
                  if (threadx >= (counts1.w << 3)) {
                    offset += counts1.w;
                    threadx -= counts1.w << 3;
                    area_log = 2;
                    lanes_log = 2;
                  }
                }
              }
            }
          }
        }
      }
    }

    const int block_index = threadx >> lanes_log;
    block = pred_blocks.Load3((offset + block_index) << 4);
    block_lane = threadx & (int)((1u << lanes_log) - 1u);
    wi_count = min(WG_SIZE, 1 << lanes_log);
    wi = block_lane & (WG_SIZE - 1);
  }

  const int bw_log = block.y & 7;
  const int bw = 4 << bw_log;
  const int bh = 1 << (area_log - bw_log);
  const int rows_log = area_log - lanes_log;
  const int rows = 1 << rows_log;

  const int bx = (block.x & 0xffff) << 2;
  const int by = (block.x >> 16) << 2;
  const int plane = (block.y >> 3) & 3;
  const int above_available = ((block.y >> 10) & 63) << 1;
  const int left_available = ((block.y >> 16) & 63) << 2;
  const int mode = (block.y >> 6) & 15;
  const bool is_dc = mode == MODE_DC;
  const bool non_skip = (block.y & (1 << 5)) != 0;

  const int4 plane_cb = plane == 0 ? cb_planes[0] : plane == 1 ? cb_planes[1] : cb_planes[2];
  const int stride = plane_cb.x;
#ifdef INTRA_8BIT
  const int corner = plane_cb.y + (bx - 4) + (by - 1) * stride;
#else
  const int corner = plane_cb.y + ((bx << 1) - 4) + (by - 1) * stride;
#endif
  const int x0 = (block_lane & (int)((1u << bw_log) - 1u)) << 2;
  const int y0 = (block_lane >> bw_log) << rows_log;
#ifdef INTRA_8BIT
  const int base_addr = corner + 4 + x0 + (y0 + 1) * stride;
#else
  const int base_addr = corner + 4 + (x0 << 1) + (y0 + 1) * stride;
#endif
  const int res_stride = plane_cb.z;
  const int base_res_addr = plane_cb.w + ((bx + x0) << 1) + (by + y0) * res_stride;

  // Issue residual row 0 load together with edge loads so latencies overlap.
  int2 res0 = 0;
  if (active && non_skip) {
    res0 = residuals.Load2(base_res_addr);
  }

  const int local_id = (int)(thread.x & (WG_SIZE - 1));
  const int local_start = local_id - wi;
  const int dc_above = above_available ? (bw >> 1) : 0;
  const int dc_left = left_available ? bh : 0;
  const int dc_terms = max(dc_above, dc_left);

  uint2 top_px = uint2(0, 0);
  int partial_sum = 0;

  if (active) {
    if (is_dc) {
      int a_sum = 0;
      int l_sum = 0;
      uint pair = 0;
      const bool do_a = wi < dc_above;
      const bool do_l = wi < dc_left;
      if (do_a) pair = load_above_pair(corner, wi, above_available);
      if (do_l) l_sum = load_left(corner, stride, wi, left_available);
      if (do_a) a_sum = (int)(pair & 1023) + (int)((pair >> 16) & 1023);
      partial_sum = a_sum + l_sum;
    } else if (mode == MODE_V) {
      if (above_available) {
        const int pair = x0 >> 1;
        top_px = uint2(load_above_pair(corner, pair, above_available),
                       load_above_pair(corner, pair + 1, above_available));
      } else {
        uint sample = left_available ? (uint)load_left(corner, stride, 0, left_available) : (uint)(PIXEL_MID - 1);
        sample |= sample << 16;
        top_px = uint2(sample, sample);
      }
    }
  }

  // Fast wave64 reduction for >= 16x16 DC blocks (wi_count == 64): zero LDS needed.
  const bool wave_full_block = (wi_count == WG_SIZE) && (WaveGetLaneCount() >= WG_SIZE) && active && is_dc;
  uint dc_val = PIXEL_MID;
  if (wave_full_block) {
    const uint total = (uint)WaveActiveSum(partial_sum);
    const uint count = (uint)(dc_above * 2 + dc_left);
    dc_val = count ? fast_dc_div(total, count) : PIXEL_MID;
  } else {
    dc_scratch[local_id] = partial_sum;
  }

  GroupMemoryBarrierWithGroupSync();

  if (active && is_dc && !wave_full_block) {
    if (wi == 0) {
      uint total = 0;
      [loop] for (int i = 0; i < dc_terms; ++i) total += (uint)dc_scratch[local_start + i];
      const uint count = (uint)(dc_above * 2 + dc_left);
      dc_scratch[local_start] = (int)(count ? fast_dc_div(total, count) : PIXEL_MID);
    }
  }

  GroupMemoryBarrierWithGroupSync();

  if (!active) return;

  if (is_dc) {
    if (!wave_full_block) dc_val = (uint)dc_scratch[local_start];
    const uint packed = dc_val | (dc_val << 16);
    top_px = uint2(packed, packed);
  }

  // Fast path for DC and V_PRED (same predicted pixels on all rows).
  if (is_dc || mode == MODE_V) {
    uint2 px0 = non_skip ? add_res_and_pack(top_px, res0) : top_px;
#ifdef INTRA_8BIT
    dst_frame.Store(base_addr, pack_samples(px0));
#else
    dst_frame.Store2(base_addr, px0);
#endif
    if (rows > 1) {
      [loop] for (int r = 1; r < rows; ++r) {
        uint2 px = top_px;
        if (non_skip) {
          int2 res = residuals.Load2(base_res_addr + r * res_stride);
          px = add_res_and_pack(px, res);
        }
#ifdef INTRA_8BIT
        dst_frame.Store(base_addr + r * stride, pack_samples(px));
#else
        dst_frame.Store2(base_addr + r * stride, px);
#endif
      }
    }
    return;
  }

  // H_PRED and Smooth / SmoothV / SmoothH (loaded here so DC/V keep minimal live registers).
  const bool sv = (mode == MODE_SMOOTH || mode == MODE_SMOOTH_V);
  const bool sh = (mode == MODE_SMOOTH || mode == MODE_SMOOTH_H);
  int smooth_bottom = 0;
  int smooth_right = 0;
  if (sv) {
    if (above_available) {
      const int pair = x0 >> 1;
      top_px = uint2(load_above_pair(corner, pair, above_available),
                     load_above_pair(corner, pair + 1, above_available));
    } else {
      uint sample = left_available ? (uint)load_left(corner, stride, 0, left_available) : (uint)(PIXEL_MID - 1);
      sample |= sample << 16;
      top_px = uint2(sample, sample);
    }
    if (left_available) smooth_bottom = load_left(corner, stride, bh - 1, left_available);
    else if (above_available) smooth_bottom = (int)(dst_frame.Load(corner + 4) & PIXEL_MAX);
    else smooth_bottom = PIXEL_MID + 1;
  }
  if (sh) {
    if (above_available) smooth_right = (int)(load_above_pair(corner, (bw >> 1) - 1, above_available) >> 16) & PIXEL_MAX;
    else if (left_available) smooth_right = load_left(corner, stride, 0, left_available);
    else smooth_right = PIXEL_MID - 1;
  }

  const bool need_h_left = (mode == MODE_H || sh);
  const int fallback_left = (need_h_left && !left_available)
      ? (above_available ? (int)(dst_frame.Load(corner + 4) & PIXEL_MAX) : (PIXEL_MID + 1))
      : 0;
  const int shift = (mode == MODE_SMOOTH) ? 9 : 8;

  [loop] for (int r = 0; r < rows; ++r) {
    const int y = y0 + r;
    uint2 px = uint2(0, 0);
    int left = 0;
    if (need_h_left) {
      left = left_available ? load_left(corner, stride, y, left_available) : fallback_left;
    }
    if (mode == MODE_H) {
      const uint s = (uint)left | ((uint)left << 16);
      px = uint2(s, s);
    } else {
      const int vw = sv ? cb_sm_weight_arrays[bh + y].x : 0;
      const int bterm = sv ? (256 - vw) * smooth_bottom : 0;
      [loop] for (int i = 0; i < 4; ++i) {
        int sum = bterm;
        if (sv) {
          const uint w = (i < 2) ? top_px.x : top_px.y;
          const int top_i = (int)((w >> ((i & 1) * 16)) & 1023);
          sum += vw * top_i;
        }
        if (sh) sum += 256 * smooth_right + cb_sm_weight_arrays[bw + x0 + i].x * (left - smooth_right);
        const uint p = (uint)clamp((sum + (1 << (shift - 1))) >> shift, 0, PIXEL_MAX) << ((i & 1) * 16);
        if (i < 2) px.x |= p;
        else px.y |= p;
      }
    }
    if (non_skip) {
      const int2 res = (r == 0) ? res0 : residuals.Load2(base_res_addr + r * res_stride);
      px = add_res_and_pack(px, res);
    }
#ifdef INTRA_8BIT
    dst_frame.Store(base_addr + r * stride, pack_samples(px));
#else
    dst_frame.Store2(base_addr + r * stride, px);
#endif
  }
}
