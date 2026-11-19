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

namespace legacy {
#include "legacy.gen.h"
#include "legacy.entry.h"
#include "shader_adapter.h"
}

namespace optimized {
#include "optimized.gen.h"
#include "optimized.entry.h"
#include "shader_adapter.h"
}

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
    const int type = kind == RESTORE_SWITCHABLE ? sample(3) : kind;
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

static bool check(const Case &c, uint64_t order) {
  auto reference = c.pixels;
  auto result = c.pixels;
  if (!checked_dispatch(c, legacy::dispatch, reference, order, "legacy") ||
      !checked_dispatch(c, optimized::dispatch, result, ~order, "optimized")) {
    return false;
  }
  const auto mismatch = std::mismatch(reference.begin(), reference.end(), result.begin());
  if (mismatch.first == reference.end()) return true;
  fprintf(stderr, "mismatch at byte %zu: legacy %u, optimized %u\n",
          static_cast<size_t>(mismatch.first - reference.begin()), *mismatch.first, *mismatch.second);
  return false;
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
        for (int kind : {-1, RESTORE_NONE, RESTORE_WIENER, RESTORE_SGRPROJ, RESTORE_SWITCHABLE}) {
          const Size size = kind == RESTORE_SWITCHABLE ? grid : edges;
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
  printf("restoration_check: %d planes match the legacy shader at 8 and 10 bits\n", cases);
}
