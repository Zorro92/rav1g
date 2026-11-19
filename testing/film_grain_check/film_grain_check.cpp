// SPDX-License-Identifier: BSD-2-Clause

// Run the direct-texture apply shaders and the copy of ungrained/neutral chroma on the host.
#define GROUP_SCHED_IMPLEMENTATION
#include "hlsl.h"

#include <algorithm>
#include <array>
#include <cstdio>
#include <random>
#include <vector>

template <class T>
struct Texture {
  int width, height;
  std::vector<T> samples;
  std::vector<uint8_t> writes;
  T discarded{};

  Texture(int w, int h) : width(w), height(h), samples(w * h), writes(w * h) {}

  T &store(int2 pos) {
    // D3D drops stores that extend beyond the texture at the end of a row.
    if (pos.x >= width || pos.y >= height) return discarded;
    if (pos.x < 0 || pos.y < 0) std::abort();
    const int i = pos.y * width + pos.x;
    if (++writes[i] != 1) {
      std::fprintf(stderr, "FAIL: %s writes pixel (%d,%d) twice\n", gs_state.shader, pos.x, pos.y);
      std::exit(1);
    }
    return samples[i];
  }
};

template <class T>
struct RWTexture2D {
  Texture<T> *texture;
  T &operator[](int2 pos) const { return texture->store(pos); }
};

// HLSL also accepts an int shift count for unsigned vectors.
template <int N>
vec<uint, N> operator<<(const vec<uint, N> &v, int shift) {
  return v << uint(shift);
}

long g_out_of_bounds = 0;

// Start legacy shared memory with nonzero data while checking that every read has a writer.
static void scramble_bytes(void *buffer, size_t size, uint64_t *state) {
  auto *bytes = static_cast<uint8_t *>(buffer);
  for (size_t i = 0; i < size; ++i) {
    *state = *state * 6364136223846793005ULL + 1;
    bytes[i] = *state >> 56;
  }
}

#include "film_grain_const.gen.h"
#include "film_grain_apply.gen.h"

namespace legacy {
#define SEMI_PLANAR
#define SP_TEXTURE
#define SP_LUMA_REGISTER u0
#define SP_CHROMA_REGISTER u1
#include "film_grain_filter.gen.h"
#include "film_grain_filter.entry.h"
#undef SP_CHROMA_REGISTER
#undef SP_LUMA_REGISTER
#undef SP_TEXTURE
#undef SEMI_PLANAR
}

namespace optimized {
#include "film_grain_filter_sp_tex_v2.gen.h"
#include "film_grain_filter_sp_tex_v2.entry.h"
#undef SP_CHROMA_REGISTER
#undef SP_LUMA_REGISTER
#undef SP_TEXTURE
}

namespace copy_plane {
#define SP_TEXTURE
#include "copy_plane_sp.gen.h"
#include "copy_plane_sp.entry.h"
#undef SP_CHROMA_REGISTER
#undef SP_LUMA_REGISTER
#undef SP_TEXTURE
}

enum Grain { LumaOnly, Monochrome, Both, ChromaFromLuma, ChromaOnly, LumaAndCb };
constexpr const char *kGrainNames[] = {"luma-only", "mono", "Y/Cb/Cr", "chroma from luma", "chroma-only", "Y/Cb"};

struct Frame {
  Texture<uint> y;
  Texture<uint2> uv;

  Frame(int w, int h) : y((w + 1) & ~1, (h + 1) & ~1), uv((w + 1) / 2, (h + 1) / 2) {}
};

struct Case {
  FilmGrainData constants;
  int padded_width, padded_height;
  Grain mode;
  std::vector<uint8_t> pixels;
  std::vector<int> grain, offsets;
  size_t offset_count;  // Entries initialized for the image, before slack for the original dispatch.
};

static int ceil_div(int n, int d) { return (n + d - 1) / d; }

static void make_scaling(Case &test, int plane, int count, std::mt19937 &rng) {
  auto &lut = test.constants.scaling_lut;
  if (count == 0) return;
  std::array<int, 256> keys;
  for (int i = 0; i < 256; ++i) keys[i] = i;
  std::shuffle(keys.begin(), keys.end(), rng);
  std::sort(keys.begin(), keys.begin() + count);
  int start = 0;
  int scale = rng() & 255;
  for (int point = 0; point + 1 < count; ++point) {
    const int next_key = keys[point + 1];
    const int next_scale = rng() & 255;
    const int dx = next_key - keys[point];
    const int delta = (next_scale - scale) * ((65536 + (dx >> 1)) / dx);
    for (; start < next_key; ++start) {
      const int t = max(start - keys[point], 0);
      lut[start][plane] = scale + ((t * delta + 32768) >> 16);
    }
    scale = next_scale;
  }
  for (; start < 256; ++start) lut[start][plane] = scale;
}

static Case make_case(int width, int height, uint depth, Grain mode, bool overlap, bool restricted, bool identity,
                      std::mt19937 &rng) {
  Case test{};
  auto &data = test.constants;
  auto &p = data.params;
  test.padded_width = ceil_div(width, 8) * 8;
  test.padded_height = ceil_div(height, 8) * 8;
  test.mode = mode;
  data.width = width;
  data.height = height;
  data.enable_chroma = mode >= Both;
  data.mc_identity = identity;
  p.num_y_points = mode == ChromaOnly ? 0 : 1 + rng() % 14;
  p.num_cb_points = (mode == Both || mode == ChromaOnly || mode == LumaAndCb) ? 1 + rng() % 10 : 0;
  p.num_cr_points = (mode == Both || mode == ChromaOnly) ? 1 + rng() % 10 : 0;
  p.chroma_scaling_from_luma = mode == ChromaFromLuma;
  p.bit_depth = depth;
  p.scaling_shift = 8 + rng() % 4;
  p.clip_to_restricted_range = restricted;
  p.overlap_flag = overlap;
  p.cb_mult = rng() & 255;
  p.cb_luma_mult = rng() & 255;
  p.cb_offset = rng() & 511;
  p.cr_mult = rng() & 255;
  p.cr_luma_mult = rng() & 255;
  p.cr_offset = rng() & 511;
  p.random_seed = rng() & 65535;

  make_scaling(test, 0, p.num_y_points, rng);
  if (p.chroma_scaling_from_luma) {
    for (int i = 0; i < 256; ++i) {
      data.scaling_lut[i].y = data.scaling_lut[i].x;
      data.scaling_lut[i].z = data.scaling_lut[i].x;
    }
  } else {
    make_scaling(test, 1, p.num_cb_points, rng);
    make_scaling(test, 2, p.num_cr_points, rng);
  }

  // Real template geometry with randomized, in-range grain samples. The generators are shared by both paths.
  data.left_pad = data.top_pad = data.right_pad = data.ar_padding = 3;
  data.luma_grain_stride = 82;
  data.chroma_grain_stride = 44;
  data.grain_offset_u = 82 * 73;
  data.grain_offset_v = data.grain_offset_u + 44 * 38;
  const int center = 1 << (depth - 1);
  // The original Y-overlap pass speculatively fetches upper-left samples across a row,
  // even though only the first column uses them. Use the decoder's padded buffer size.
  test.grain.resize(96 * 96 + 48 * 48 * 2);
  for (int &value : test.grain) value = int(rng() & (2 * center - 1)) - center;

  data.random_offset_stride = test.padded_width / 32 + 1;
  const int offset_rows = test.padded_height / 32 + 1;
  test.offset_count = data.random_offset_stride * offset_rows;
  test.offsets.resize(test.offset_count + 2 * data.random_offset_stride);
  for (int &offset : test.offsets) offset = rng() & 255;
  for (int y = 0; y < ceil_div(test.padded_height, 32); ++y) {
    uint state = p.random_seed ^ (((y * 37 + 178) & 255) << 8) ^ ((y * 173 + 105) & 255);
    for (int x = 0; x < ceil_div(test.padded_width, 32); ++x) {
      const uint bit = (state ^ (state >> 1) ^ (state >> 3) ^ (state >> 12)) & 1;
      state = (state >> 1) | (bit << 15);
      test.offsets[y * data.random_offset_stride + x] = state >> 8;
    }
  }

  // The old dispatch may read whole blocks beyond the visible frame. Provide backing storage there.
  const int bytes = depth == 8 ? 1 : 2;
  for (int plane = 0; plane < 3; ++plane) {
    const int width = ceil_div((96 * data.random_offset_stride) >> (plane != 0), 16) * 16;
    const int height = (32 * offset_rows) >> (plane != 0);
    const int offset = test.pixels.size() + 128;
    const int stride = width * bytes + 64;
    data.src_planes[plane] = int4(stride, offset, 0, 0);
    test.pixels.resize(offset + stride * height);
    for (int y = 0; y < height; ++y) {
      for (int x = 0; x < width; ++x) {
        // Exercise the last 8-bit knot and all four 10-bit indices that map to it.
        const uint value = (x % 16 == 0) ? (2 * center - 1 - (y & 3)) : rng() & (2 * center - 1);
        auto *pixel = &test.pixels[offset + y * stride + x * bytes];
        pixel[0] = value & 255;
        if (bytes == 2) pixel[1] = value >> 8;
      }
    }
  }
  return test;
}

struct Dispatch {
  void (*entry)(uint3, uint3);
  uint3 threads;
  uint3 group;

  static void run(int thread, void *context) {
    const auto &d = *static_cast<const Dispatch *>(context);
    d.entry(d.group, uint3(thread % d.threads.x, (thread / d.threads.x) % d.threads.y,
                          thread / (d.threads.x * d.threads.y)));
  }
};

static void dispatch(void (*entry)(uint3, uint3), uint3 threads, int nx, int ny, uint64_t &order) {
  Dispatch d{entry, threads, {}};
  for (int y = 0; y < ny; ++y) {
    for (int x = 0; x < nx; ++x) {
      d.group = uint3(x, y, 0);
      gs_run_group(threads.x * threads.y * threads.z, Dispatch::run, &d, &order);
    }
  }
}

static void copy_chroma(const Case &test, Frame &frame, uint64_t &order) {
  using namespace copy_plane;
  pool = {const_cast<uint8_t *>(test.pixels.data()), test.pixels.size()};
  sp_luma = {&frame.y};
  sp_chroma = {&frame.uv};
  cb_width = test.constants.width;
  cb_height = test.constants.height;
  cb_groups = ceil_div(frame.uv.width, 8);
  cb_rows = frame.uv.height;
  cb_hbd = test.constants.params.bit_depth == 10;
  cb_mode = test.mode == Monochrome ? SP_MODE_CHROMA_NEUTRAL : SP_MODE_CHROMA;
  cb_src_stride = test.constants.src_planes[1].x;
  cb_src_offset = test.constants.src_planes[1].y;
  cb_src_offset_v = test.constants.src_planes[2].y;
  dispatch(shader_entry, uint3(kThreadsX, kThreadsY, kThreadsZ), ceil_div(cb_groups, kThreadsX),
           ceil_div(cb_rows, kThreadsY), order);
}

static bool same(uint a, uint b) { return a == b; }
static bool same(uint2 a, uint2 b) { return a.x == b.x && a.y == b.y; }

template <class T>
static void compare(const Case &test, const Texture<T> &expected, const Texture<T> &actual, const char *plane) {
  const auto &data = test.constants;
  for (size_t i = 0; i < expected.samples.size(); ++i) {
    if (expected.writes[i] == 1 && actual.writes[i] == 1 && same(expected.samples[i], actual.samples[i])) continue;
    std::fprintf(stderr,
                 "FAIL: pixel mismatch %s (%zu,%zu), %dx%d %u-bit %s, seed %u, overlap %d, clip %d, stores %u/%u\n",
                 plane, i % actual.width, i / actual.width, data.width, data.height, data.params.bit_depth,
                 kGrainNames[test.mode], data.params.random_seed, data.params.overlap_flag,
                 data.params.clip_to_restricted_range, expected.writes[i], actual.writes[i]);
    std::exit(1);
  }
}

// Pack the optimized buffer using the interpolation formula of the reference HLSL.
static void pack_p010_scaling(FilmGrainData &data) {
  for (int i = 0; i < 256; ++i) {
    for (int plane = 0; plane < 3; ++plane) {
      const int scale = data.scaling_lut[i][plane];
      const int next = data.scaling_lut[min(i + 1, 255)][plane];
      uint word = 0;
      for (int fraction = 0; fraction < 4; ++fraction) {
        const int interpolated = scale + (((next - scale) * fraction + 2) >> 2);
        word |= uint(interpolated) << (8 * fraction);
      }
      data.scaling_lut[i][plane] = int(word);
    }
  }
}

static void check_case(const Case &test, uint64_t &order) {
  const auto &data = test.constants;
  Frame expected(data.width, data.height), actual(data.width, data.height);
  legacy::data = data;
  legacy::src = {test.pixels.data(), test.pixels.size()};
  legacy::grain_block = {test.grain.data(), test.grain.size()};
  legacy::random_offset = {test.offsets.data(), test.offsets.size()};
  legacy::sp_luma = {&expected.y};
  legacy::sp_chroma = {&expected.uv};
  gs_state.shader = "film_grain_filter_sp_tex";
  legacy::shader_scramble(&order);
  dispatch(legacy::shader_entry, uint3(legacy::kThreadsX, legacy::kThreadsY, legacy::kThreadsZ),
           data.random_offset_stride, test.padded_height / 32 + 1, order);

  optimized::data = data;
  if (data.params.bit_depth == 10) pack_p010_scaling(optimized::data);
  optimized::src = legacy::src;
  const size_t grain_samples = data.grain_offset_v + data.chroma_grain_stride * 38;
  optimized::grain_block = {reinterpret_cast<const uint8_t *>(test.grain.data()), grain_samples * sizeof(int)};
  optimized::random_offset = {test.offsets.data(), test.offset_count};
  optimized::sp_luma = {&actual.y};
  optimized::sp_chroma = {&actual.uv};
  gs_state.shader = "film_grain_filter_sp_tex_v2";
  dispatch(optimized::shader_entry, uint3(optimized::kThreadsX, optimized::kThreadsY, optimized::kThreadsZ),
           ceil_div(data.width, 32), ceil_div(data.height, 32), order);
  if (!data.enable_chroma) {
    gs_state.shader = "copy_plane_sp_tex";
    copy_chroma(test, expected, order);
    copy_chroma(test, actual, order);
  }

  if (gs_state.races || gs_state.sync_errors || gs_state.out_of_bounds || g_out_of_bounds) {
    gs_print_races("  ");
    std::fprintf(stderr, "FAIL: %ld group memory races, %ld sync errors, %ld out-of-bounds reads\n",
                 gs_state.races, gs_state.sync_errors, gs_state.out_of_bounds + g_out_of_bounds);
    std::exit(1);
  }
  compare(test, expected.y, actual.y, "Y");
  compare(test, expected.uv, actual.uv, "UV");
}

int main() {
  std::mt19937 rng(0xe6ba47fd);
  uint64_t order = 0xbf3d544853dafe55;
  gs_state.track_unwritten = true;
  int count = 0;
  for (uint depth : {8u, 10u}) {
    for (bool overlap : {false, true}) {
      for (bool restricted : {false, true}) {
        for (Grain mode : {LumaOnly, Monochrome, Both, ChromaFromLuma, ChromaOnly, LumaAndCb}) {
          // Exercise each combination at the first block, both edges of a block, and partial blocks.
          for (const auto &size : {int2(1, 1), int2(32, 32), int2(33, 35), int2(68, 67),
                                  int2(65 + rng() % 40, 34 + rng() % 40)}) {
            check_case(make_case(size.x, size.y, depth, mode, overlap, restricted, (count & 1) != 0, rng), order);
            ++count;
          }
        }
      }
    }
  }
  std::printf("PASS: %d NV12/P010 cases matched at every luma and chroma texel\n", count);
}
