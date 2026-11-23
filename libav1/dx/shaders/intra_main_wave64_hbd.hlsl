/*
 * Wave64-native full intra prediction shader (10-bit; 8-bit when INTRA_8BIT is defined).
 *
 * Sized for the console's wave64 compute units (AGENTS.md):
 * - WG_SIZE = 64 (1 wave64/group): zero s_barrier on RDNA2, 2560 B LDS (4x smaller than
 *   the 256-thread shader), 57 VGPRs (8 waves/SIMD, 0 spills), 4x finer mode isolation
 *   between blocks in mixed-mode wavefront steps.
 * - Large blocks (>16x16) pack 2 or 4 rows per lane (matching intra_simple so host counts
 *   and indirect arguments are identical across both shaders).
 * - Exact directional reach and Zone 1/3 corner pruning match AV1 7.11.2.4 and the host's
 *   IntraMain::Wave64 wavefront dependencies.
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

#define Dir1 1
#define Dir2 2
#define Dir3 3
#define SmoothV 4
#define SmoothH 8

#define MODE_PAETH 11
#define MODE_INTRA_BC 12
#define MODE_DC 14
#define MODE_CFL 15

#define NeedAboveShift 4
#define NeedRightShift 5
#define NeedLeftShift 6
#define NeedBotShift 7
#define NeedAboveLeftShift 8
#define FilterAboveLeftFlag 0x200
#define SubpelBits 4
#define FilterLineShift 3
#define OutputShift 11
#define OffsetBits 19
#define SumAddHor (1 << 14)
#define SumAddVert ((1 << OffsetBits) + (1 << (OutputShift - 1)))
#define OutputSub ((1 << (OffsetBits - OutputShift)) + (1 << (OffsetBits - OutputShift - 1)))

#define WG_SIZE 64

#ifdef INTRA_8BIT
#define PIXEL_MAX 255
#define PIXEL_MID 128
#define LEFT_SHIFT 24
#else
#define PIXEL_MAX 1023
#define PIXEL_MID 512
#define LEFT_SHIFT 16
#endif

groupshared int edges[WG_SIZE * 10];

int4 get_plane_cb(int plane) {
  return plane == 0 ? cb_planes[0] : plane == 1 ? cb_planes[1] : cb_planes[2];
}

int reduction_stride(int base, int wi, int wi_count, int terms) {
  if (terms < 32) return 1;
  const int lanes = (int)WaveGetLaneCount();
  if (lanes < 32 || wi >= lanes) return 1;
  if (WaveActiveAllEqual(base) && WaveActiveCountBits(true) == (uint)min(lanes, wi_count)) return lanes;
  return 1;
}

int sum_partials(int base, int wi, int count, int step) {
  int sum = 0;
  [loop] for (int i = wi; i < count; i += step) sum += edges[base + i];
  if (step > 1) sum = WaveActiveSum(sum);
  return sum;
}

uint fast_dc_div(uint sum, uint count) {
  const uint shift = firstbitlow(count);
  const uint odd = count >> shift;
  uint s = (sum + (count >> 1)) >> shift;
  if (odd != 1) {
    const uint mult = (odd == 3) ? 21846u : 13108u;
    s = (s * mult) >> 16;
  }
  return s;
}

int filter_edge(int base, int i, int last, int strength) {
  const int outer = (0x2000 >> (4 * strength)) & 15;
  const int inner = (0x4540 >> (4 * strength)) & 15;
  const int center = (0x4680 >> (4 * strength)) & 15;
  return (8 + outer * (edges[base + max(i - 1, 0)] + edges[base + min(i + 3, last)]) +
          inner * (edges[base + i] + edges[base + min(i + 2, last)]) +
          center * edges[base + i + 1]) >> 4;
}

int upsample_edge(int p0, int p1, int p2, int p3) {
  return clamp((-p0 + 9 * (p1 + p2) - p3 + 8) >> 4, 0, PIXEL_MAX);
}

void prepare_bank(int raw, int prepared, int wi, int wi_count, int count, int prep_count, int upsample,
                  int filter_count, int strength, int need_corner) {
  if (raw == prepared) return;
  if (wi == 0 && need_corner) {
    edges[prepared - 2] = edges[raw - 2];
    if (!upsample) edges[prepared - 1] = edges[raw - 1];
  }
  if (upsample) {
    if (wi >= ((prep_count + 1) >> 1)) return;
    const int i = wi * 2;
    const int p0 = edges[raw + i - 2];
    const int p1 = edges[raw + i - 1];
    const int p2 = edges[raw + i];
    const int p3 = edges[raw + i + 1];
    const int p4 = edges[raw + min(i + 2, count - 1)];
    edges[prepared + 2 * i - 1] = upsample_edge(p0, p1, p2, p3);
    edges[prepared + 2 * i] = p2;
    edges[prepared + 2 * i + 1] = upsample_edge(p1, p2, p3, p4);
    edges[prepared + 2 * i + 2] = p3;
  } else {
    [loop] for (int i = wi; i < prep_count; i += wi_count) {
      edges[prepared + i] = i < filter_count
        ? filter_edge(raw - need_corner, i, filter_count, strength)
        : edges[raw + i];
    }
  }
}

#ifdef INTRA_8BIT
uint load_above_pair(int corner, int wi, int available) {
  const int at = min(wi, available - 1);
  const uint word = dst_frame.Load(corner + 4 + 4 * (at >> 1));
  const uint half_word = (at & 1) ? word >> 16 : word & 0xffff;
  uint pair = (half_word & 255) | ((half_word >> 8) << 16);
  if (wi >= available) pair = (pair & 0xffff0000) | (pair >> 16);
  return pair;
}

uint2 unpack_samples(uint word) {
  return uint2((word & 255) | (((word >> 8) & 255) << 16), ((word >> 16) & 255) | ((word >> 24) << 16));
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

uint2 predict_straight(bool vertical, int corner, int stride, int x, int y, int above, int left) {
  [branch] if (vertical && above) {
    const int pair = x >> 1;
    return uint2(load_above_pair(corner, pair, above), load_above_pair(corner, pair + 1, above));
  }
  uint sample = vertical ? PIXEL_MID - 1 : PIXEL_MID + 1;
  [branch] if (left) sample = load_left(corner, stride, vertical ? 0 : y, left);
  else [branch] if (above) sample = dst_frame.Load(corner + 4) & PIXEL_MAX;
  sample |= sample << 16;
  return uint2(sample, sample);
}

int compute_bc(int p0, int p1, int p2, int p3, int fh, int fv) {
  int l0 = ((1 << 14) + p0 * fh + p1 * (128 - fh) + (1 << (FilterLineShift - 1))) >> FilterLineShift;
  int l1 = ((1 << 14) + p2 * fh + p3 * (128 - fh) + (1 << (FilterLineShift - 1))) >> FilterLineShift;
  int output = SumAddVert + l0 * fv + l1 * (128 - fv);
  return clamp((output >> OutputShift) - OutputSub, 0, PIXEL_MAX);
}

int intra_mode_flags(int mode, int mode_angle) {
  if (mode < 8) {
    const uint nominal = ((mode < 4 ? 0x872DB45Au : 0x43CB9D71u) >> (8 * (mode & 3))) & 255;
    const int angle = (int)nominal + 3 * (mode_angle - 3);
    return angle < 90 ? 305 : angle == 90 ? 273 : angle < 180 ? 850 : angle == 180 ? 323 : 451;
  }
  return mode == 8 ? 92 : mode == 9 ? 84 : mode == 10 ? 88 : mode == MODE_PAETH ? 336 : mode == MODE_INTRA_BC ? 0 : 80;
}

#ifdef INTRA_8BIT
uint2 predict_intra_bc(uint block_z, int4 plane_cb, int x, int y) {
  const int stride = plane_cb.x;
  const int mv = (int)block_z;
  const int mvx = x + (mv >> (16 + SubpelBits));
  const int mvy = y + ((mv << 16) >> (16 + SubpelBits));
  const int filt_h = 128 >> ((mv >> 19) & 1);
  const int filt_v = 128 >> ((mv >> 3) & 1);
  int addr = plane_cb.y + mvx + mvy * stride;
  uint2 ref0, ref1 = 0;

  const uint shift = (addr & 3) << 3;
  addr &= ~3;
  ref0 = dst_frame.Load2(addr);
  ref0.x = (ref0.x >> shift) | ((ref0.y << (24 - shift)) << 8);
  ref0.y = ref0.y >> shift;

  if (filt_v != 128) {
    ref1 = dst_frame.Load2(addr + stride);
    ref1.x = (ref1.x >> shift) | ((ref1.y << (24 - shift)) << 8);
    ref1.y = ref1.y >> shift;
  }

  uint2 pixels;
  pixels.x = compute_bc((ref0.x >> 0) & 255, (ref0.x >> 8) & 255, (ref1.x >> 0) & 255, (ref1.x >> 8) & 255, filt_h,
                        filt_v) |
             (compute_bc((ref0.x >> 8) & 255, (ref0.x >> 16) & 255, (ref1.x >> 8) & 255, (ref1.x >> 16) & 255,
                         filt_h, filt_v)
              << 16);
  pixels.y = compute_bc((ref0.x >> 16) & 255, (ref0.x >> 24) & 255, (ref1.x >> 16) & 255, (ref1.x >> 24) & 255,
                        filt_h, filt_v) |
             (compute_bc((ref0.x >> 24) & 255, (ref0.y >> 0) & 255, (ref1.x >> 24) & 255, (ref1.y >> 0) & 255,
                         filt_h, filt_v)
              << 16);
  return pixels;
}
#else
uint2 predict_intra_bc(uint block_z, int4 plane_cb, int x, int y) {
  const int stride = plane_cb.x;
  const int mv = (int)block_z;
  const int mvx = x + (mv >> (16 + SubpelBits));
  const int mvy = y + ((mv << 16) >> (16 + SubpelBits));
  const int filt_h = 128 >> ((mv >> 19) & 1);
  const int filt_v = 128 >> ((mv >> 3) & 1);
  int addr = plane_cb.y + (mvx << 1) + mvy * stride;
  uint3 ref0, ref1 = 0;

  const uint shift = (addr & 2) << 3;
  addr &= ~3;
  ref0 = dst_frame.Load3(addr);
  ref0.x = (ref0.x >> shift) | ((ref0.y << (24 - shift)) << 8);
  ref0.y = (ref0.y >> shift) | ((ref0.z << (24 - shift)) << 8);
  ref0.z = ref0.z >> shift;

  if (filt_v != 128) {
    ref1 = dst_frame.Load3(addr + stride);
    ref1.x = (ref1.x >> shift) | ((ref1.y << (24 - shift)) << 8);
    ref1.y = (ref1.y >> shift) | ((ref1.z << (24 - shift)) << 8);
    ref1.z = ref1.z >> shift;
  }

  uint2 pixels = uint2(0, 0);
  [loop] for (int p = 0; p < 2; ++p) {
    const uint r0a = (p == 0) ? ref0.x : ref0.y;
    const uint r0b = (p == 0) ? ref0.y : ref0.z;
    const uint r1a = (p == 0) ? ref1.x : ref1.y;
    const uint r1b = (p == 0) ? ref1.y : ref1.z;
    const uint a0 = r0a & 1023;
    const uint a1 = (r0a >> 16) & 1023;
    const uint a2 = r0b & 1023;
    const uint b0 = r1a & 1023;
    const uint b1 = (r1a >> 16) & 1023;
    const uint b2 = r1b & 1023;
    const uint pair = compute_bc(a0, a1, b0, b1, filt_h, filt_v) |
                      (compute_bc(a1, a2, b1, b2, filt_h, filt_v) << 16);
    if (p == 0) pixels.x = pair;
    else pixels.y = pair;
  }
  return pixels;
}
#endif

uint compute_cfl_pixel(int dc, int value) {
  value = (value < 0) ? -((-value + (1 << 5)) >> 6) : ((value + (1 << 5)) >> 6);
  return clamp(dc + value, 0, PIXEL_MAX);
}

int intra_inter_blend(int mask, int intra, int inter) {
  return clamp((intra * mask + inter * (64 - mask) + 32) >> 6, 0, PIXEL_MAX);
}

#ifdef INTRA_8BIT
int4 cfl_luma_row(uint limits, int bx, int by, int x0, int y) {
  int4 luma;
  const int max_x = (limits & 0xffff) - 4;
  const int max_y = (limits >> 16) - 2;
  const int y_stride = cb_planes[0].x;
  const int luma_x = (bx + x0) << 1;
  const int luma_y = min((by + y) << 1, max_y);
  const int y_offset = cb_planes[0].y + min(luma_x, max_x) + luma_y * y_stride;
  uint2 luma0 = dst_frame.Load2(y_offset);
  uint2 luma1 = dst_frame.Load2(y_offset + y_stride);
  if (luma_x >= max_x) {
    luma0.y = (luma0.x >> 16) | (luma0.x & 0xffff0000);
    luma1.y = (luma1.x >> 16) | (luma1.x & 0xffff0000);
    if (luma_x > max_x) {
      luma0.x = luma0.y;
      luma1.x = luma1.y;
    }
  }
  luma.x = ((luma0.x >> 0) & 255) + ((luma0.x >> 8) & 255) + ((luma1.x >> 0) & 255) + ((luma1.x >> 8) & 255);
  luma.y = ((luma0.x >> 16) & 255) + ((luma0.x >> 24) & 255) + ((luma1.x >> 16) & 255) + ((luma1.x >> 24) & 255);
  luma.z = ((luma0.y >> 0) & 255) + ((luma0.y >> 8) & 255) + ((luma1.y >> 0) & 255) + ((luma1.y >> 8) & 255);
  luma.w = ((luma0.y >> 16) & 255) + ((luma0.y >> 24) & 255) + ((luma1.y >> 16) & 255) + ((luma1.y >> 24) & 255);
  luma <<= 1;
  return luma;
}
#else
int cfl_pair_sum(uint a, uint b) {
  return (((int)(a & 1023) + (int)((a >> 16) & 1023)) + ((int)(b & 1023) + (int)((b >> 16) & 1023))) << 1;
}

int4 cfl_luma_row(uint limits, int bx, int by, int x0, int y) {
  const int max_y = (limits >> 16) - 2;
  const int y_stride = cb_planes[0].x;
  const int luma_y = min((by + y) << 1, max_y);
  const int max_x = ((limits & 0xffff) << 1) - 8;
  const int luma_x = (bx + x0) << 2;
  const int y_offset = cb_planes[0].y + min(luma_x, max_x) + luma_y * y_stride;

  uint2 a01 = dst_frame.Load2(y_offset);
  uint2 b01 = dst_frame.Load2(y_offset + y_stride);
  if (luma_x > max_x) {
    a01.x = a01.y;
    b01.x = b01.y;
  }
  const int s0 = cfl_pair_sum(a01.x, b01.x);
  const int s1 = cfl_pair_sum(a01.y, b01.y);
  int s2 = s1;
  int s3 = s1;
  if (luma_x < max_x) {
    const uint2 a23 = dst_frame.Load2(y_offset + 8);
    const uint2 b23 = dst_frame.Load2(y_offset + y_stride + 8);
    s2 = cfl_pair_sum(a23.x, b23.x);
    s3 = cfl_pair_sum(a23.y, b23.y);
  }
  return int4(s0, s1, s2, s3);
}
#endif

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

  const int local_start = (int)(thread.x & (WG_SIZE - 1)) - wi;
  const int raw_above = local_start * 10 + 2;
  const int raw_stride = (wi_count >> 1) * 5;
  const int raw_left = raw_above + raw_stride;
  const int rows_log = area_log - lanes_log;
  const int rows = 1 << rows_log;

  const int mode = (block.y >> 6) & 15;
  const int mode_angle = (block.y >> 28) & 7;
  const bool straight = mode < 2 && mode_angle == 3 && active;
  const bool wave_full_dc = (wi_count == WG_SIZE) && (WaveGetLaneCount() >= WG_SIZE) && active && (mode >= MODE_DC);

  // Phase 1: load edges or compute DC / CfL partials.
  // Keep directional setup strictly scoped to its branch so VGPRs stay low during CfL.
  int dc_partial = 0;
  int cfl_partial = 0;
  if (active && mode >= MODE_DC) {
    const int bw_log = block.y & 7;
    const int bw = 4 << bw_log;
    const int bh = 1 << (area_log - bw_log);
    const int above_available = ((block.y >> 10) & 63) << 1;
    const int left_available = ((block.y >> 16) & 63) << 2;
    const int dc_above = above_available ? bw >> 1 : 0;
    const int dc_left = left_available ? bh : 0;
    const int dc_terms = max(dc_above, dc_left);
    const int bx1 = (block.x & 0xffff) << 2;
    const int by1 = (block.x >> 16) << 2;
    const int4 plane_cb = get_plane_cb((block.y >> 3) & 3);
    const int stride = plane_cb.x;
#ifdef INTRA_8BIT
    const int corner = plane_cb.y + (bx1 - 4) + (by1 - 1) * stride;
#else
    const int corner = plane_cb.y + ((bx1 << 1) - 4) + (by1 - 1) * stride;
#endif
    if (wi < dc_terms) {
      uint pair = 0;
      int l_sum = 0;
      const bool do_a = wi < dc_above;
      const bool do_l = wi < dc_left;
      if (do_a) pair = load_above_pair(corner, wi, above_available);
      if (do_l) l_sum = load_left(corner, stride, wi, left_available);
      const int a_sum = do_a ? (int)(pair & 1023) + (int)((pair >> 16) & 1023) : 0;
      dc_partial = a_sum + l_sum;
      if (!wave_full_dc) edges[raw_above + wi] = dc_partial;
    }
    if (mode == MODE_CFL && ((block.y >> 22) & 63) != 16) {
      const int x0 = (block_lane & (int)((1u << bw_log) - 1u)) << 2;
      const int y0 = (block_lane >> bw_log) << rows_log;
      [loop] for (int r = 0; r < rows; ++r) {
        const int4 lr = cfl_luma_row(block.z, bx1, by1, x0, y0 + r);
        cfl_partial += lr.x + lr.y + lr.z + lr.w;
      }
      if (!wave_full_dc) edges[raw_left + wi] = cfl_partial;
    }
  } else if (active && mode < MODE_INTRA_BC && !straight) {
    const int bw_log = block.y & 7;
    const int bw = 4 << bw_log;
    const int bh = 1 << (area_log - bw_log);
    const int above_available = ((block.y >> 10) & 63) << 1;
    const int left_available = ((block.y >> 16) & 63) << 2;
    const int mflags = intra_mode_flags(mode, mode_angle);
    const int minfo = mflags & 15;
    const bool directional = minfo >= Dir1 && minfo <= Dir3;
    const int above_count = ((mflags >> NeedAboveShift) & 1) * bw + ((mflags >> NeedRightShift) & 1) * bh;
    const int left_count = ((mflags >> NeedLeftShift) & 1) * bh + ((mflags >> NeedBotShift) & 1) * bw;
    int need_aboveleft = (mflags >> NeedAboveLeftShift) & 1;

    int load_above_count = above_count;
    int load_left_count = left_count;
    bool read_first_above = minfo != Dir3 || left_available == 0;
    bool read_first_left = minfo != Dir1 || above_available == 0;
    const bool corner_filt = (mflags & FilterAboveLeftFlag) != 0 && bw + bh >= 24 && cb_flags.x != 0;

    if (directional) {
      const int2 ds = cb_mode_params_lut[mode][mode_angle].xy;
      const int up_a = (block.y >> 22) & 1;
      const int up_l = (block.y >> 23) & 1;
      const int str_a = (block.y >> 24) & 3;
      const int str_l = (block.y >> 26) & 3;
      if (minfo == Dir1) {
        if (!up_a && !str_a) need_aboveleft = 0;
        const int max_pred = (bw - 1) + ((bh * ds.x) >> 6) + 1;
        const int pad = (up_a || str_a) ? 2 : 0;
        load_above_count = min(above_count, (max_pred + pad + 2) & ~1);
      } else if (minfo == Dir3) {
        if (!up_l && !str_l) need_aboveleft = 0;
        const int max_pred = (bh - 1) + ((bw * ds.y) >> 6) + 1;
        const int pad = (up_l || str_l) ? 2 : 0;
        load_left_count = min(left_count, max_pred + pad + 1);
      } else {
        const bool any_above = ((bw - 1) << 6) + ds.x >= -64;
        const bool any_left = bh * ds.x < -64;
        if (!any_above && !corner_filt) {
          load_above_count = 0;
          read_first_above = left_available == 0;
        }
        if (!any_left && !corner_filt) {
          load_left_count = 0;
          read_first_left = above_available == 0;
        }
      }
    }

    const int bx1 = (block.x & 0xffff) << 2;
    const int by1 = (block.x >> 16) << 2;
    const int4 plane_cb = get_plane_cb((block.y >> 3) & 3);
    const int stride = plane_cb.x;
#ifdef INTRA_8BIT
    const int corner = plane_cb.y + (bx1 - 4) + (by1 - 1) * stride;
#else
    const int corner = plane_cb.y + ((bx1 << 1) - 4) + (by1 - 1) * stride;
#endif

    const bool do_above = wi < (load_above_count >> 1) || (wi == 0 && read_first_above);
    const bool do_left0 = wi < load_left_count || (wi == 0 && read_first_left);
    const bool do_left1 = wi + wi_count < load_left_count;
    const bool do_corner = wi == 0 && need_aboveleft != 0;

    uint above_pair = (PIXEL_MID - 1) | ((PIXEL_MID - 1) << 16);
    int left_val0 = PIXEL_MID + 1;
    int left_val1 = PIXEL_MID + 1;
    int corner_val = PIXEL_MID;

    if (do_above) {
      if (above_available) {
        above_pair = load_above_pair(corner, wi, above_available);
      } else if (left_available) {
        const uint p = (uint)load_left(corner, stride, 0, left_available) & 1023;
        above_pair = p | (p << 16);
      }
    }
    if (do_left0) {
      if (left_available) left_val0 = load_left(corner, stride, wi, left_available);
      else if (above_available) left_val0 = (int)(dst_frame.Load(corner + 4) & PIXEL_MAX);
    }
    if (do_left1) {
      if (left_available) left_val1 = load_left(corner, stride, wi + wi_count, left_available);
      else if (above_available) left_val1 = (int)(dst_frame.Load(corner + 4) & PIXEL_MAX);
    }
    if (do_corner && left_available && above_available) {
      corner_val = dst_frame.Load(corner) >> LEFT_SHIFT;
    }

    if (do_above) {
      edges[raw_above + wi * 2] = (int)(above_pair & 1023);
      edges[raw_above + wi * 2 + 1] = (int)((above_pair >> 16) & 1023);
    }
    if (do_left0) edges[raw_left + wi] = left_val0;
    if (do_left1) edges[raw_left + wi + wi_count] = left_val1;
    if (do_corner) {
      const int first_above = (int)(above_pair & 1023);
      if (!left_available || !above_available) {
        corner_val = left_available ? left_val0 : above_available ? first_above : PIXEL_MID;
      }
      if (corner_filt) corner_val = ((left_val0 + first_above) * 5 + corner_val * 6 + 8) >> 4;
      edges[raw_above - 1] = corner_val;
      edges[raw_above - 2] = corner_val;
      edges[raw_left - 1] = corner_val;
      edges[raw_left - 2] = corner_val;
    }
  }

  if (wave_full_dc) {
    const int bw_log = block.y & 7;
    const int bw = 4 << bw_log;
    const int bh = 1 << (area_log - bw_log);
    const int above_available = ((block.y >> 10) & 63) << 1;
    const int left_available = ((block.y >> 16) & 63) << 2;
    const uint total = (uint)WaveActiveSum(dc_partial);
    const uint count = (uint)((above_available ? bw : 0) + (left_available ? bh : 0));
    if (wi == 0) edges[raw_above - 1] = (int)(count ? fast_dc_div(total, count) : PIXEL_MID);
    if (mode == MODE_CFL && ((block.y >> 22) & 63) != 16) {
      const int ctotal = WaveActiveSum(cfl_partial);
      if (wi == 0) edges[raw_left - 1] = (ctotal + (2 << area_log)) >> (area_log + 2);
    }
  }

  GroupMemoryBarrierWithGroupSync();

  // Phase 2: directional edge filtering/upsampling or sub-wave64 DC/CfL reduction.
  int prepared_above = raw_above;
  int prepared_left = raw_left;
  if (active && mode < 8 && !straight) {
    const int bw_log = block.y & 7;
    const int bw = 4 << bw_log;
    const int bh = 1 << (area_log - bw_log);
    const int above_available = ((block.y >> 10) & 63) << 1;
    const int left_available = ((block.y >> 16) & 63) << 2;
    const int mflags = intra_mode_flags(mode, mode_angle);
    const int minfo = mflags & 15;
    const int need_above = (mflags >> NeedAboveShift) & 1;
    const int need_right = (mflags >> NeedRightShift) & 1;
    const int need_left = (mflags >> NeedLeftShift) & 1;
    const int need_bot = (mflags >> NeedBotShift) & 1;
    int need_aboveleft = (mflags >> NeedAboveLeftShift) & 1;
    const int above_count = need_above * bw + need_right * bh;
    const int left_count = need_left * bh + need_bot * bw;
    const int up_a = (block.y >> 22) & 1;
    const int up_l = (block.y >> 23) & 1;
    const int str_a = (block.y >> 24) & 3;
    const int str_l = (block.y >> 26) & 3;
    const bool corner_filt = (mflags & FilterAboveLeftFlag) != 0 && bw + bh >= 24 && cb_flags.x != 0;
    const int2 ds = cb_mode_params_lut[mode][mode_angle].xy;

    int prep_above_count = above_count;
    int prep_left_count = left_count;
    if (minfo == Dir1) {
      if (!up_a && !str_a) need_aboveleft = 0;
      const int max_pred = (bw - 1) + ((bh * ds.x) >> 6) + 1;
      prep_above_count = min(above_count, max_pred + 1);
    } else if (minfo == Dir3) {
      if (!up_l && !str_l) need_aboveleft = 0;
      const int max_pred = (bh - 1) + ((bw * ds.y) >> 6) + 1;
      prep_left_count = min(left_count, max_pred + 1);
    } else {
      if (((bw - 1) << 6) + ds.x < -64 && !corner_filt) prep_above_count = 0;
      if (bh * ds.x >= -64 && !corner_filt) prep_left_count = 0;
    }

    const int filter_above = above_available && need_above
                                 ? min(above_available << 1, bw) + need_right * bh + need_aboveleft - 1
                                 : 0;
    const int filter_left = left_available && need_left
                                ? min(left_available, bh) + need_bot * bw + need_aboveleft - 1
                                : 0;
    const int bank_start = raw_left + raw_stride;
    const bool process_above = prep_above_count && (up_a || (str_a && filter_above));
    const bool process_left = prep_left_count && (up_l || (str_l && filter_left));
    prepared_above = process_above ? bank_start : raw_above;
    prepared_left = process_left ? bank_start + (process_above ? raw_stride : 0) : raw_left;

    prepare_bank(raw_above, prepared_above, wi, wi_count, above_count, prep_above_count, up_a,
                 filter_above, str_a, need_aboveleft);
    prepare_bank(raw_left, prepared_left, wi, wi_count, left_count, prep_left_count, up_l,
                 filter_left, str_l, need_aboveleft);
  } else if (active && mode >= MODE_DC && !wave_full_dc) {
    const int bw_log = block.y & 7;
    const int bw = 4 << bw_log;
    const int bh = 1 << (area_log - bw_log);
    const int above_available = ((block.y >> 10) & 63) << 1;
    const int left_available = ((block.y >> 16) & 63) << 2;
    const int dc_above = above_available ? bw >> 1 : 0;
    const int dc_left = left_available ? bh : 0;
    const int dc_terms = max(dc_above, dc_left);
    const bool cfl = mode == MODE_CFL && ((block.y >> 22) & 63) != 16;
    const int step = reduction_stride(raw_above, wi, wi_count, cfl ? wi_count : dc_terms);
    if (step > 1 || wi == 0) {
      const uint count = (uint)(dc_above * 2 + dc_left);
      const uint dc = (uint)sum_partials(raw_above, wi, dc_terms, step);
      if (wi == 0) edges[raw_above - 1] = (int)(count ? fast_dc_div(dc, count) : PIXEL_MID);
      if (cfl) {
        const int total = sum_partials(raw_left, wi, wi_count, step);
        if (wi == 0) edges[raw_left - 1] = (total + (2 << area_log)) >> (area_log + 2);
      }
    }
  }

  GroupMemoryBarrierWithGroupSync();
  if (!active) return;

  // Phase 3: predict, blend, add residual, store (rows = 1, 2, or 4).
  const int4 plane_cb = get_plane_cb((block.y >> 3) & 3);
  const int stride = plane_cb.x;
  const int bw_log = block.y & 7;
  const int bx3 = (block.x & 0xffff) << 2;
  const int by3 = (block.x >> 16) << 2;
  const int bw3 = 4 << bw_log;
  const int bh3 = 1 << (area_log - bw_log);
  const int x0 = (block_lane & (int)((1u << bw_log) - 1u)) << 2;
  const int y0 = (block_lane >> bw_log) << rows_log;
  const bool dir3 = !straight && mode < 8;
#ifdef INTRA_8BIT
  const int corner = plane_cb.y + (bx3 - 4) + (by3 - 1) * stride;
  const int base_addr = corner + 4 + x0 + (y0 + 1) * stride;
#else
  const int corner = plane_cb.y + ((bx3 << 1) - 4) + (by3 - 1) * stride;
  const int base_addr = corner + 4 + (x0 << 1) + (y0 + 1) * stride;
#endif

  [loop] for (int r = 0; r < rows; ++r) {
    const int y = y0 + r;
    const int addr = base_addr + r * stride;

    uint2 pixels = uint2(0, 0);
    if (straight) {
      const int av = ((block.y >> 10) & 63) << 1;
      const int lv = ((block.y >> 16) & 63) << 2;
      pixels = predict_straight(mode == 0, corner, stride, x0, y, av, lv);
    } else if (mode == MODE_INTRA_BC) {
      pixels = predict_intra_bc(block.z, plane_cb, bx3 + x0, by3 + y);
    } else if (mode >= MODE_DC) {
      const uint dc = (uint)edges[raw_above - 1];
      const bool cfl3 = mode == MODE_CFL && ((block.y >> 22) & 63) != 16;
      if (cfl3) {
        const int avrg = edges[raw_left - 1];
        const int alpha = ((block.y >> 22) & 63) - 16;
        const int4 lr = cfl_luma_row(block.z, bx3, by3, x0, y);
        pixels.x = compute_cfl_pixel(dc, (lr.x - avrg) * alpha) |
                   (compute_cfl_pixel(dc, (lr.y - avrg) * alpha) << 16);
        pixels.y = compute_cfl_pixel(dc, (lr.z - avrg) * alpha) |
                   (compute_cfl_pixel(dc, (lr.w - avrg) * alpha) << 16);
      } else {
        pixels.x = dc | (dc << 16);
        pixels.y = pixels.x;
      }
    } else if (dir3) {
      const int mflags3 = intra_mode_flags(mode, mode_angle);
      const int minfo3 = mflags3 & 15;
      const int up_a3 = (block.y >> 22) & 1;
      const int up_l3 = (block.y >> 23) & 1;
      const int above_cnt3 = ((mflags3 >> NeedAboveShift) & 1) * bw3 + ((mflags3 >> NeedRightShift) & 1) * bh3;
      const int left_cnt3 = ((mflags3 >> NeedLeftShift) & 1) * bh3 + ((mflags3 >> NeedBotShift) & 1) * bw3;
      const int2 dir_steps = cb_mode_params_lut[mode][mode_angle].xy;
      const int frac_bits_x = 6 - up_a3;
      const int frac_bits_y = 6 - up_l3;
      const int min_base_x = -(1 << up_a3);
      const int last_x = (above_cnt3 << up_a3) - 1;
      const int last_y = (left_cnt3 << up_l3) - 1;
      const int dx = (y + 1) * dir_steps.x;
      const int frac_x = ((dx << up_a3) & 63) >> 1;
      [loop] for (int i = 0; i < 4; ++i) {
        const int x = x0 + i;
        const int base_x = (x << up_a3) + (dx >> frac_bits_x);
        const bool from_above = minfo3 == Dir1 || (minfo3 == Dir2 && base_x >= min_base_x);
        const int dy = (y << 6) + (x + 1) * dir_steps.y;
        const int base = from_above ? base_x : (dy >> frac_bits_y);
        const int fraction = from_above ? frac_x : (((dy << up_l3) & 63) >> 1);
        const int last = from_above ? last_x : last_y;
        const int edge_base = from_above ? prepared_above : prepared_left;
        const int p0 = edges[edge_base + clamp(base, -2, last)];
        int val = p0;
        if (fraction) {
          const int p1 = edges[edge_base + clamp(base + 1, -2, last)];
          val = p0 + ((fraction * (p1 - p0) + 16) >> 5);
        }
        const uint packed = (uint)clamp(val, 0, PIXEL_MAX) << ((i & 1) * 16);
        if (i < 2) pixels.x |= packed;
        else pixels.y |= packed;
      }
    } else if (mode == MODE_PAETH) {
      const int left = edges[raw_left + y];
      const int corner_pixel = edges[raw_above - 1];
      const int diff_top = corner_pixel - left;
      const int dt = abs(diff_top);
      [loop] for (int i = 0; i < 4; ++i) {
        const int top = edges[raw_above + x0 + i];
        const int diff_left = corner_pixel - top;
        const int diff_corner = abs(diff_top + diff_left);
        const int dl = abs(diff_left);
        const int val = dl <= dt && dl <= diff_corner ? left : dt <= diff_corner ? top : corner_pixel;
        const uint packed = (uint)clamp(val, 0, PIXEL_MAX) << ((i & 1) * 16);
        if (i < 2) pixels.x |= packed;
        else pixels.y |= packed;
      }
    } else {
      const bool sv = (mode == 8 || mode == 9);
      const bool sh = (mode == 8 || mode == 10);
      const int vertical_weight = sv ? cb_sm_weight_arrays[bh3 + y].x : 0;
      const int bottom_term = sv ? (256 - vertical_weight) * edges[raw_left + bh3 - 1] : 0;
      const int left = sh ? edges[raw_left + y] : 0;
      const int right = sh ? edges[raw_above + bw3 - 1] : 0;
      const int shift = (mode == 8) ? 9 : 8;
      [loop] for (int i = 0; i < 4; ++i) {
        int sum = bottom_term;
        if (sv) sum += vertical_weight * edges[raw_above + x0 + i];
        if (sh) sum += 256 * right + cb_sm_weight_arrays[bw3 + x0 + i].x * (left - right);
        const uint packed = (uint)clamp((sum + (1 << (shift - 1))) >> shift, 0, PIXEL_MAX) << ((i & 1) * 16);
        if (i < 2) pixels.x |= packed;
        else pixels.y |= packed;
      }
    }

    if (block.y & 0x80000000) {
      const uint wedge = wedge_mask.Load((block.z << 6) + x0 + y * bw3);
#ifdef INTRA_8BIT
      const uint2 inter = unpack_samples(dst_frame.Load(addr));
#else
      const uint2 inter = dst_frame.Load2(addr);
#endif
      pixels.x = intra_inter_blend((wedge >> 0) & 255, (pixels.x >> 0) & 1023, (inter.x >> 0) & 1023) |
                 (intra_inter_blend((wedge >> 8) & 255, (pixels.x >> 16) & 1023, (inter.x >> 16) & 1023) << 16);
      pixels.y = intra_inter_blend((wedge >> 16) & 255, (pixels.y >> 0) & 1023, (inter.y >> 0) & 1023) |
                 (intra_inter_blend((wedge >> 24) & 255, (pixels.y >> 16) & 1023, (inter.y >> 16) & 1023) << 16);
    }

    if (block.y & (1 << 5)) {
      const int2 res = residuals.Load2(plane_cb.w + ((bx3 + x0) << 1) + (by3 + y) * plane_cb.z);
      pixels.x = clamp((int)((pixels.x >> 0) & 1023) + (int)((res.x << 16) >> 16), 0, PIXEL_MAX) |
                 (clamp((int)((pixels.x >> 16) & 1023) + (int)(res.x >> 16), 0, PIXEL_MAX) << 16);
      pixels.y = clamp((int)((pixels.y >> 0) & 1023) + (int)((res.y << 16) >> 16), 0, PIXEL_MAX) |
                 (clamp((int)((pixels.y >> 16) & 1023) + (int)(res.y >> 16), 0, PIXEL_MAX) << 16);
    }

#ifdef INTRA_8BIT
    dst_frame.Store(addr, pack_samples(pixels));
#else
    dst_frame.Store2(addr, pixels);
#endif
  }
}
