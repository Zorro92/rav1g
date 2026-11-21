// SPDX-License-Identifier: BSD-2-Clause

#include <algorithm>
#include <array>
#include <random>
#include <vector>

#define GROUP_SCHED_IMPLEMENTATION
#include "hlsl.h"

long g_out_of_bounds;

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
}
