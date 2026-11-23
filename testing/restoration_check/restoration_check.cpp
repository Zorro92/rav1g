// SPDX-License-Identifier: BSD-2-Clause

#include <algorithm>
#include <array>
#include <random>
#include <vector>

#define GROUP_SCHED_IMPLEMENTATION
#include "hlsl.h"

long g_out_of_bounds;

template <int N>
vec<uint, N> operator<<(const vec<uint, N> &v, int shift) {
  return v << uint(shift);
}

template <class T>
struct Texture {
  uint width = 0, height = 0;
  std::vector<T> samples;
  std::vector<uint> writes;
  T discarded{};
  Texture() = default;
  Texture(uint w, uint h) : width(w), height(h), samples(w * h), writes(w * h) {}
  T &store(int2 pos) {
    if (pos.x < 0 || pos.y < 0 || uint(pos.x) >= width || uint(pos.y) >= height) return discarded;
    const uint i = uint(pos.y) * width + uint(pos.x);
    ++writes[i];
    return samples[i];
  }
};

template <class T>
struct RWTexture2D {
  Texture<T> *texture = nullptr;
  T &operator[](int2 pos) const { return texture->store(pos); }
};

static void scramble_bytes(void *buffer, size_t size, uint64_t *state) {
  auto *bytes = static_cast<uint8_t *>(buffer);
  for (size_t i = 0; i < size; ++i) {
    *state = *state * 6364136223846793005ULL + 1;
    bytes[i] = static_cast<uint8_t>(*state >> 56);
  }
}

struct Size {
  int width;
  int height;
};

struct Case {
  Size plane;
  int subsampling, bit_depth, unit_size;
  bool use_units;
  int stride, src_offset, pre_offset, dst_offset;
  int dispatch_width, dispatch_height;
  int rows, cols, first_unit;
  std::vector<std::array<int, 4>> types;
  std::vector<std::array<int, 16>> taps;
  std::vector<uint8_t> pixels;
};

struct sgr_params_type {
  int radius[2];
  int strength[2];
};

#include "restoration.gen.h"
#include "sgr.gen.h"

namespace optimized {
#include "optimized.gen.h"
#include "optimized.entry.h"
#include "shader_adapter.h"
}

namespace wiener {
// The same specialization as loop_restoration_wiener.hlsl.
#define LR_WIENER_ONLY
#include "optimized.gen.h"
#include "optimized.entry.h"
#include "shader_adapter.h"
#undef LR_WIENER_ONLY
}

namespace sp_tex {
#define LR_OUTPUT_SP_TEX
#include "optimized.gen.h"
#include "optimized.entry.h"
#undef LR_OUTPUT_SP_TEX
#undef SP_TEXTURE
#undef SP_LUMA_REGISTER
#undef SP_CHROMA_REGISTER
#undef LR_FLAG_WRITE_FB
#undef LR_FLAG_OUT_LUMA
#undef LR_FLAG_OUT_CHROMA
#undef LR_FLAG_MONOCHROME
#undef LR_FLAG_COMBINED_CHROMA

static void run_thread(int thread, void *context) {
  const auto &group = *static_cast<uint3 *>(context);
  shader_entry(group, uint3(thread % kThreadsX, thread / kThreadsX, 0));
}

static void init_sgr() {
  for (int i = 0; i < SGRPROJ_PARAMS; ++i) {
    const auto &param = sgr_params[i];
    data.Sgr_Params[i] = int4(param.radius[0], param.radius[1], param.strength[0], param.strength[1]);
    for (int j = 0; j < 4; ++j) {
      uint word = 0;
      for (int byte = 0; byte < 4; ++byte) {
        word |= static_cast<uint>(x_by_xplus1[i * 16 + j * 4 + byte] - 1) << (8 * byte);
      }
      data.Sgr_A2Packed[i][j] = word;
    }
  }
}

static void dispatch_luma(const Case &c, std::vector<uint8_t> &result, Texture<uint> &tex_y, int flags, uint64_t order) {
  init_sgr();
  plane_id = 0;
  do_restoration = c.use_units;
  out_flags = flags;
  pl[0].plane = {c.stride, c.src_offset, c.plane.width, c.plane.height};
  pl[0].units = {c.rows, c.cols, c.unit_size, c.cols};
  pl[0].pp_offset = c.pre_offset;
  pl[0].dst_offset = c.dst_offset;
  pl[0].Lr_buffer_offset = c.first_unit;
  pl[0].subsampling = 0;
  pl[0].hbd = c.bit_depth > 8;
  pl[0].bit_depth = c.bit_depth;
  pl[0].unit_shift = __builtin_ctz(c.unit_size);
  src = {result.data(), result.size()};
  dst = {result.data(), result.size()};
  LrTypeSgr = {reinterpret_cast<const uint8_t *>(c.types.data()), c.types.size() * sizeof(c.types[0])};
  LrWiener = {reinterpret_cast<const uint8_t *>(c.taps.data()), c.taps.size() * sizeof(c.taps[0])};
  sp_luma = {&tex_y};
  shader_scramble(&order);
  for (int y = 0; y < c.dispatch_height; y += kThreadsY) {
    for (int x = 0; x < c.dispatch_width; x += kThreadsX) {
      uint3 group(x / kThreadsX, y / kThreadsY, 0);
      gs_run_group(kThreadsX * kThreadsY, run_thread, &group, &order);
    }
  }
}

static void dispatch_chroma(const Case &cu, const Case &cv, std::vector<uint8_t> &pool,
                            const std::vector<std::array<int, 4>> &types,
                            const std::vector<std::array<int, 16>> &taps,
                            int u_first, int v_first, int u_base, int v_base,
                            Texture<uint2> &tex_uv, int flags, uint64_t order) {
  init_sgr();
  plane_id = 1;
  do_restoration = (cu.use_units ? 1 : 0) | (cv.use_units ? 2 : 0);
  out_flags = flags;
  auto setup = [&](int pid, const Case &c, int first, int base) {
    pl[pid].plane = {c.stride, base + c.src_offset, c.plane.width, c.plane.height};
    pl[pid].units = {c.rows, c.cols, c.unit_size, c.cols};
    pl[pid].pp_offset = base + c.pre_offset;
    pl[pid].dst_offset = base + c.dst_offset;
    pl[pid].Lr_buffer_offset = first;
    pl[pid].subsampling = 1;
    pl[pid].hbd = c.bit_depth > 8;
    pl[pid].bit_depth = c.bit_depth;
    pl[pid].unit_shift = __builtin_ctz(c.unit_size);
  };
  setup(1, cu, u_first, u_base);
  setup(2, cv, v_first, v_base);
  src = {pool.data(), pool.size()};
  dst = {pool.data(), pool.size()};
  LrTypeSgr = {reinterpret_cast<const uint8_t *>(types.data()), types.size() * sizeof(types[0])};
  LrWiener = {reinterpret_cast<const uint8_t *>(taps.data()), taps.size() * sizeof(taps[0])};
  sp_chroma = {&tex_uv};
  shader_scramble(&order);
  for (int y = 0; y < cu.dispatch_height; y += kThreadsY) {
    for (int x = 0; x < cu.dispatch_width; x += kThreadsX) {
      uint3 group(x / kThreadsX, y / kThreadsY, 0);
      gs_run_group(kThreadsX * kThreadsY, run_thread, &group, &order);
    }
  }
}
}

static constexpr int kSwitchable = 3, kWienerOrNone = 4;

static Case make_case(Size size, int ss, int depth, int unit_size, int kind, int param, int pattern,
                      std::mt19937 &random) {
  Case c{};
  c.plane = size;
  c.subsampling = ss;
  c.bit_depth = depth;
  c.use_units = kind != -1;
  c.unit_size = c.use_units ? unit_size : 1;
  c.dispatch_width = (size.width + 15) & ~15;
  c.dispatch_height = (size.height + 3) & ~3;
  const int bpp = depth > 8 ? 2 : 1;
  c.stride = c.dispatch_width * bpp + 32;
  c.src_offset = 16;
  const int plane_size = c.stride * c.dispatch_height;
  c.pre_offset = c.src_offset + plane_size;
  c.dst_offset = c.pre_offset + plane_size;
  c.pixels.resize(c.dst_offset + plane_size, 0xA5);

  auto sample = [&](int bound) { return static_cast<int>(random() % bound); };
  const int max_sample = (1 << depth) - 1;
  for (int base : {c.src_offset, c.pre_offset}) {
    for (int y = 0; y < c.dispatch_height; ++y) {
      for (int x = 0; x < c.dispatch_width; ++x) {
        int value = sample(max_sample + 1);
        if (x < size.width && y < size.height) {
          // Exercise z = 0, low nonzero variances and saturation, with independent stripe sources.
          const int center = base == c.src_offset ? max_sample / 3 : max_sample * 2 / 3;
          switch (pattern) {
            case 0: value = center; break;
            case 1: value = center + sample(5) - 2; break;
            case 2: value = center + sample(40) - 20; break;
            case 3: value = ((x + y + (base == c.pre_offset)) & 1) * max_sample; break;
          }
        }
        const int at = base + y * c.stride + x * bpp;
        c.pixels[at] = static_cast<uint8_t>(value);
        if (bpp == 2) c.pixels[at + 1] = static_cast<uint8_t>(value >> 8);
      }
    }
  }

  c.rows = c.use_units ? std::max(1, (size.height + unit_size / 2) / unit_size) : 1;
  c.cols = c.use_units ? std::max(1, (size.width + unit_size / 2) / unit_size) : 1;
  if (!c.use_units) return c;  // A type-table access on a copy-only plane is an error.

  c.first_unit = 3;
  c.types.resize(c.first_unit + c.rows * c.cols);
  c.taps.resize(c.types.size());
  for (size_t i = c.first_unit; i < c.types.size(); ++i) {
    const int type = kind == kSwitchable ? sample(3) : kind == kWienerOrNone ? sample(2) : kind;
    const int ep = (param + i - c.first_unit) % SGRPROJ_PARAMS;
    int w0 = sgr_params[ep].radius[0] ? sample(128) + SGRPROJ_PRJ_MIN0 : 0;
    int w1 = sgr_params[ep].radius[1] ? sample(128) + SGRPROJ_PRJ_MIN1 : SGRPROJ_PRJ_MAX1;
    c.types[i] = {type, w0, w1, ep};
    for (int axis : {0, 8}) {
      const int t0 = ss ? 0 : WIENER_FILT_TAP0_MINV + sample(1 << WIENER_FILT_TAP0_BITS);
      const int t1 = WIENER_FILT_TAP1_MINV + sample(1 << WIENER_FILT_TAP1_BITS);
      const int t2 = WIENER_FILT_TAP2_MINV + sample(1 << WIENER_FILT_TAP2_BITS);
      const int filter[8] = {t0, t1, t2, -2 * (t0 + t1 + t2), t2, t1, t0, 0};
      std::copy_n(filter, 8, c.taps[i].begin() + axis);
    }
  }
  return c;
}

static int round_shift(int64_t value, int bits) {
  return int((value + ((int64_t(1) << bits) / 2)) >> bits);
}

static int read_sample(const Case &c, int base, int x, int y) {
  const bool hbd = c.bit_depth > 8;
  const uint8_t *p = &c.pixels[base + y * c.stride + x * (hbd ? 2 : 1)];
  return hbd ? p[0] + 256 * p[1] : p[0];
}

// AV1 7.17.6. Each processing stripe draws its halo from the unfiltered picture.
struct Stripe {
  const Case &c;
  int first, last;

  int sample(int x, int y) const {
    x = std::clamp(x, 0, c.plane.width - 1);
    y = std::clamp(y, 0, c.plane.height - 1);
    const bool outside = y < first || y > last;
    return read_sample(c, outside ? c.pre_offset : c.src_offset, x, std::clamp(y, first - 2, last + 2));
  }
};

static int reference_wiener(const Stripe &stripe, int x, int y, const std::array<int, 16> &taps) {
  // AV1 7.17.4–5. Expand the center taps and convolve in signed sample space.
  const int offset = 1 << (stripe.c.bit_depth + 3);
  int value = 0;
  for (int dy = -3; dy <= 3; ++dy) {
    int intermediate = 0;
    for (int dx = -3; dx <= 3; ++dx) {
      intermediate += (taps[dx + 3] + (dx == 0 ? 128 : 0)) * stripe.sample(x + dx, y + dy);
    }
    intermediate = std::clamp(round_shift(intermediate, 3), -offset, offset * 3 - 1);
    value += (taps[dy + 11] + (dy == 0 ? 128 : 0)) * intermediate;
  }
  return round_shift(value, 11);
}

// AV1 7.17.2–3. Compute each box directly, including the halo coefficients.
// The CPU does not use the shader's lookup table, packed coefficients or column sums.
static int reference_box(const Stripe &stripe, int x, int y, int radius, int strength, int center_sample) {
  if (radius == 0) return center_sample * 16;
  const int shift = stripe.c.bit_depth - 8;
  const int n = (radius * 2 + 1) * (radius * 2 + 1);
  const int reciprocal = (4096 + n / 2) / n;
  int combined = 0;
  for (int dy = -1; dy <= 1; ++dy) {
    if (radius == 2 && (y + dy) % 2 == 0) continue;
    for (int dx = -1; dx <= 1; ++dx) {
      int sum = 0, squares = 0;
      for (int row = -radius; row <= radius; ++row) {
        for (int col = -radius; col <= radius; ++col) {
          const int v = stripe.sample(x + dx + col, y + dy + row);
          sum += v;
          squares += v * v;
        }
      }
      const int scaled = round_shift(sum, shift);
      const int variance = std::max(0, n * round_shift(squares, 2 * shift) - scaled * scaled);
      const int z = round_shift(int64_t(variance) * strength, 20);
      const int a = z >= 255 ? 256 : z == 0 ? 1 : (256 * z + z / 2) / (z + 1);
      const int b = round_shift(int64_t(256 - a) * sum * reciprocal, 12);
      const int weight = radius == 2 ? (dx == 0 ? 6 : 5) : (dx == 0 || dy == 0 ? 4 : 3);
      combined += weight * (a * center_sample + b);
    }
  }
  return round_shift(combined, radius == 2 && y % 2 ? 8 : 9);
}

static int reference_sgr(const Stripe &stripe, int x, int y, const std::array<int, 4> &unit) {
  const auto &params = sgr_params[unit[3]];
  const int sample = stripe.sample(x, y);
  const int large = reference_box(stripe, x, y, params.radius[0], params.strength[0], sample);
  const int small = reference_box(stripe, x, y, params.radius[1], params.strength[1], sample);
  const int w0 = unit[1], w1 = unit[2];
  return round_shift(w0 * large + w1 * 16 * sample + (128 - w0 - w1) * small, 11);
}

static std::vector<uint8_t> reference_plane(const Case &c) {
  auto result = c.pixels;
  const bool hbd = c.bit_depth > 8;
  const int stripe_height = 64 >> c.subsampling, stripe_offset = 8 >> c.subsampling;
  for (int y = 0; y < c.dispatch_height; ++y) {
    const int stripe = (y + stripe_offset) / stripe_height;
    const int first = stripe * stripe_height - stripe_offset;
    const Stripe source{c, first, first + stripe_height - 1};
    for (int x = 0; x < c.dispatch_width; ++x) {
      int value = read_sample(c, c.src_offset, x, y);
      if (c.use_units) {
        // Unit ownership extends to the right and bottom of the last unit.
        const int row = std::min((y + stripe_offset) / c.unit_size, c.rows - 1);
        const int col = std::min(x / c.unit_size, c.cols - 1);
        const int index = c.first_unit + row * c.cols + col;
        const auto &unit = c.types[index];
        if (unit[0] == RESTORE_WIENER) value = reference_wiener(source, x, y, c.taps[index]);
        if (unit[0] == RESTORE_SGRPROJ) value = reference_sgr(source, x, y, unit);
      }
      const int at = c.dst_offset + y * c.stride + x * (hbd ? 2 : 1);
      value = std::clamp(value, 0, (1 << c.bit_depth) - 1);
      result[at] = uint8_t(value);
      if (hbd) result[at + 1] = uint8_t(value >> 8);
    }
  }
  return result;
}

static bool checked_dispatch(const Case &c, void (*run)(const Case &, std::vector<uint8_t> &, uint64_t),
                             std::vector<uint8_t> &result, uint64_t order, const char *shader) {
  gs_reset_report();
  gs_state.shader = shader;
  gs_state.track_unwritten = true;
  g_out_of_bounds = 0;
  run(c, result, order);
  if (g_out_of_bounds || gs_state.out_of_bounds || gs_state.races || gs_state.sync_errors ||
      gs_state.plain_barriers || gs_state.runaway_groups) {
    fprintf(stderr, "%s: invalid shader accesses or synchronization (buffer OOB %ld, shared OOB %ld, "
                    "races %ld, sync errors %ld, plain barriers %ld, runaway groups %ld)\n",
            shader, g_out_of_bounds, gs_state.out_of_bounds, gs_state.races, gs_state.sync_errors,
            gs_state.plain_barriers, gs_state.runaway_groups);
    gs_print_races("  ");
    return false;
  }
  return true;
}

static bool check_output(const Case &c, const std::vector<uint8_t> &reference, uint64_t order,
                         void (*run)(const Case &, std::vector<uint8_t> &, uint64_t), const char *shader) {
  auto result = c.pixels;
  if (!checked_dispatch(c, run, result, order, shader)) {
    return false;
  }
  const auto mismatch = std::mismatch(reference.begin(), reference.end(), result.begin());
  if (mismatch.first == reference.end()) return true;
  fprintf(stderr, "mismatch at byte %zu: reference %u, %s %u\n",
          static_cast<size_t>(mismatch.first - reference.begin()), *mismatch.first, shader, *mismatch.second);
  return false;
}

static bool check(const Case &c, uint64_t order) {
  const auto reference = reference_plane(c);
  if (!check_output(c, reference, order, optimized::dispatch, "optimized")) {
    return false;
  }
  const bool wiener_or_none = std::all_of(c.types.begin(), c.types.end(), [](const auto &unit) {
    return unit[0] == RESTORE_WIENER || unit[0] == RESTORE_NONE;
  });
  return !wiener_or_none || check_output(c, reference, ~order, wiener::dispatch, "wiener");
}

int main() {
  std::mt19937 random(0x4c52534d);
  int cases = 0;
  for (int depth : {8, 10}) {
    for (int ss : {0, 1}) {
      const int stripe = 64 >> ss;
      const int first_stripe = 56 >> ss;
      for (int param = 0; param < SGRPROJ_PARAMS; ++param) {
        const int unit_size = (64 >> ss) << (param % (ss ? 4 : 3));
        const int width = param < 4 ? param + 1 : 13 + param;
        const Size edges{width, param < 4 ? param + 1 : first_stripe - 2 + param % 5};
        const int merged_edge = unit_size * 3 / 2 + param % 4 - 1;
        const Size grid{merged_edge, std::max(stripe + first_stripe + 2, merged_edge)};
        for (int kind : {-1, RESTORE_NONE, RESTORE_WIENER, RESTORE_SGRPROJ, kSwitchable, kWienerOrNone}) {
          const Size size = kind == kSwitchable || kind == kWienerOrNone ? grid : edges;
          const Case c = make_case(size, ss, depth, unit_size, kind, param, (param / 2) % 5, random);
          if (!check(c, random())) {
            fprintf(stderr, "case %d: %dx%d depth %d subsampling %d unit %d type %d SGR set %d\n", cases,
                    size.width, size.height, depth, ss, unit_size, kind, param);
            return 1;
          }
          ++cases;
        }
      }
    }
  }
  printf("restoration_check: %d planes match the scalar AV1 reference at 8 and 10 bits\n", cases);

  // Verify LR_OUTPUT_SP_TEX: luma (with odd-dimension edge replication, write_fb=0/1) and combined
  // chroma (mixed U/V restoration types, monochrome, write_fb=0/1) under adversarial scheduling.
  int sp_cases = 0;
  for (int depth : {8, 10}) {
    const bool hbd = depth > 8;
    const uint shift = hbd ? 6u : 0u;
    const uint neutral = (hbd ? 512u : 128u) << shift;
    for (Size sz : {Size{33, 25}, Size{64, 64}, Size{71, 61}, Size{19, 17}}) {
      for (int kind : {-1, RESTORE_NONE, RESTORE_WIENER, RESTORE_SGRPROJ, kSwitchable}) {
        const Case c = make_case(sz, 0, depth, 64, kind, sp_cases % SGRPROJ_PARAMS, sp_cases % 4, random);
        const auto ref = reference_plane(c);
        for (int write_fb : {0, 1}) {
          const uint tw = uint((sz.width + 1) & ~1);
          const uint th = uint((sz.height + 1) & ~1);
          Texture<uint> tex_y(tw, th);
          auto buf = c.pixels;
          gs_reset_report();
          gs_state.shader = "loop_restoration_sp_tex (luma)";
          gs_state.track_unwritten = true;
          g_out_of_bounds = 0;
          sp_tex::dispatch_luma(c, buf, tex_y, (write_fb ? 1 : 0) | 2, random());
          if (g_out_of_bounds || gs_state.out_of_bounds || gs_state.races || gs_state.sync_errors ||
              gs_state.plain_barriers || gs_state.runaway_groups) {
            fprintf(stderr, "sp_tex luma: sync/bounds error (races %ld, sync %ld, OOB %ld)\n",
                    gs_state.races, gs_state.sync_errors, g_out_of_bounds);
            gs_print_races("  ");
            return 1;
          }
          const auto &exp_buf = write_fb ? ref : c.pixels;
          if (buf != exp_buf) {
            fprintf(stderr, "sp_tex luma fb mismatch (%dx%d depth %d kind %d write_fb %d)\n",
                    sz.width, sz.height, depth, kind, write_fb);
            return 1;
          }
          for (uint y = 0; y < th; ++y) {
            for (uint x = 0; x < tw; ++x) {
              const int sx = std::min(int(x), sz.width - 1);
              const int sy = std::min(int(y), sz.height - 1);
              const int at = c.dst_offset + sy * c.stride + sx * (hbd ? 2 : 1);
              const uint exp = uint(hbd ? (ref[at] | (ref[at + 1] << 8)) : ref[at]) << shift;
              const uint i = y * tw + x;
              if (tex_y.writes[i] != 1u || tex_y.samples[i] != exp) {
                fprintf(stderr, "sp_tex luma tex mismatch at (%u,%u) (%dx%d depth %d kind %d)\n",
                        x, y, sz.width, sz.height, depth, kind);
                return 1;
              }
            }
          }
          ++sp_cases;
        }
      }
    }

    // Combined chroma U+V dispatch across mixed restoration combinations.
    const std::pair<int, int> chroma_kinds[] = {
        {-1, -1},
        {RESTORE_WIENER, RESTORE_NONE},
        {RESTORE_NONE, RESTORE_SGRPROJ},
        {RESTORE_SGRPROJ, RESTORE_WIENER},
        {RESTORE_WIENER, RESTORE_SGRPROJ},
        {kSwitchable, kSwitchable},
    };
    for (auto [ku, kv] : chroma_kinds) {
      const Size csz{35, 29};
      const Case cu = make_case(csz, 1, depth, 32, ku, sp_cases % SGRPROJ_PARAMS, 1, random);
      const Case cv = make_case(csz, 1, depth, 32, kv, (sp_cases + 5) % SGRPROJ_PARAMS, 2, random);
      const auto ref_u = reference_plane(cu);
      const auto ref_v = reference_plane(cv);
      std::vector<std::array<int, 4>> types = cu.types;
      std::vector<std::array<int, 16>> taps = cu.taps;
      const int v_offset = int(types.size());
      types.insert(types.end(), cv.types.begin(), cv.types.end());
      taps.insert(taps.end(), cv.taps.begin(), cv.taps.end());
      if (types.empty()) {
        types.push_back({RESTORE_NONE, 0, 0, 0});
        taps.push_back({});
      }
      for (int write_fb : {0, 1}) {
        for (int mono : {0, 1}) {
          std::vector<uint8_t> pool = cu.pixels;
          const int v_base = int(pool.size());
          pool.insert(pool.end(), cv.pixels.begin(), cv.pixels.end());
          const auto orig_pool = pool;
          Texture<uint2> tex_uv(uint(csz.width), uint(csz.height));
          gs_reset_report();
          gs_state.shader = "loop_restoration_sp_tex (chroma)";
          gs_state.track_unwritten = true;
          g_out_of_bounds = 0;
          const int flags = (write_fb ? 1 : 0) | 4 | (mono ? 8 : 0) | 16;
          sp_tex::dispatch_chroma(cu, cv, pool, types, taps, cu.first_unit, v_offset + cv.first_unit, 0, v_base,
                                  tex_uv, flags, random());
          if (g_out_of_bounds || gs_state.out_of_bounds || gs_state.races || gs_state.sync_errors ||
              gs_state.plain_barriers || gs_state.runaway_groups) {
            fprintf(stderr, "sp_tex chroma: sync/bounds error (races %ld, sync %ld, OOB %ld)\n",
                    gs_state.races, gs_state.sync_errors, g_out_of_bounds);
            gs_print_races("  ");
            return 1;
          }
          if (write_fb) {
            if (!std::equal(ref_u.begin(), ref_u.end(), pool.begin()) ||
                !std::equal(ref_v.begin(), ref_v.end(), pool.begin() + v_base)) {
              fprintf(stderr, "sp_tex chroma fb mismatch (depth %d ku %d kv %d mono %d)\n", depth, ku, kv, mono);
              return 1;
            }
          } else if (pool != orig_pool) {
            fprintf(stderr, "sp_tex chroma wrote fb when write_fb=0\n");
            return 1;
          }
          for (int y = 0; y < csz.height; ++y) {
            for (int x = 0; x < csz.width; ++x) {
              const int at = cu.dst_offset + y * cu.stride + x * (hbd ? 2 : 1);
              const uint eu = mono ? neutral : (uint(hbd ? (ref_u[at] | (ref_u[at + 1] << 8)) : ref_u[at]) << shift);
              const uint ev = mono ? neutral : (uint(hbd ? (ref_v[at] | (ref_v[at + 1] << 8)) : ref_v[at]) << shift);
              const uint i = uint(y * csz.width + x);
              if (tex_uv.writes[i] != 1u || tex_uv.samples[i].x != eu || tex_uv.samples[i].y != ev) {
                fprintf(stderr, "sp_tex chroma tex mismatch at (%d,%d) depth %d ku %d kv %d mono %d\n",
                        x, y, depth, ku, kv, mono);
                return 1;
              }
            }
          }
          ++sp_cases;
        }
      }
    }
  }
  printf("restoration_check: %d fused semi-planar cases matched the scalar AV1 reference\n", sp_cases);
}
