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

#include "film_grain_const.gen.h"
#include "film_grain_apply.gen.h"

namespace optimized {
#include "film_grain_filter_sp_tex_v2.gen.h"
#include "film_grain_filter_sp_tex_v2.entry.h"
#undef SP_CHROMA_REGISTER
#undef SP_LUMA_REGISTER
#undef SP_TEXTURE
}

namespace copy_plane {
#include "copy_plane_sp_tex_v2.gen.h"
#include "copy_plane_sp_tex_v2.entry.h"
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
  Grain mode;
  std::vector<uint8_t> pixels;
  std::vector<int> grain, offsets;
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

  // Real template geometry with randomized, in-range grain samples.
  data.left_pad = data.top_pad = data.right_pad = data.ar_padding = 3;
  data.luma_grain_stride = 82;
  data.chroma_grain_stride = 44;
  data.grain_offset_u = 82 * 73;
  data.grain_offset_v = data.grain_offset_u + 44 * 38;
  const int center = 1 << (depth - 1);
  test.grain.resize(data.grain_offset_v + 44 * 38);
  for (int &value : test.grain) value = int(rng() & (2 * center - 1)) - center;

  data.random_offset_stride = ceil_div(width, 32);
  const int offset_rows = ceil_div(height, 32);
  test.offsets.resize(data.random_offset_stride * offset_rows);
  for (int y = 0; y < offset_rows; ++y) {
    uint state = p.random_seed ^ (((y * 37 + 178) & 255) << 8) ^ ((y * 173 + 105) & 255);
    for (int x = 0; x < data.random_offset_stride; ++x) {
      const uint bit = (state ^ (state >> 1) ^ (state >> 3) ^ (state >> 12)) & 1;
      state = (state >> 1) | (bit << 15);
      test.offsets[y * data.random_offset_stride + x] = state >> 8;
    }
  }

  const int bytes = depth == 8 ? 1 : 2;
  for (int plane = 0; plane < 3; ++plane) {
    const int plane_width = ceil_div(width, plane ? 2 : 1);
    const int plane_height = ceil_div(height, plane ? 2 : 1);
    // Packed shader loads may read into the padding of the last word of a row.
    const int stride = ceil_div(plane_width, 16) * 16 * bytes + 64;
    const int offset = test.pixels.size() + 128;
    data.src_planes[plane] = int4(stride, offset, 0, 0);
    test.pixels.resize(offset + stride * plane_height);
    for (int y = 0; y < plane_height; ++y) {
      for (int x = 0; x < stride / bytes; ++x) {
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
  cb_luma_group_rows = 0;
  cb_hbd = test.constants.params.bit_depth == 10;
  cb_neutral_chroma = test.mode == Monochrome;
  cb_u_offset = test.constants.src_planes[1].y;
  cb_v_offset = test.constants.src_planes[2].y;
  cb_uv_stride = test.constants.src_planes[1].x;
  dispatch(shader_entry, uint3(kThreadsX, kThreadsY, kThreadsZ),
           2 * ceil_div(cb_width, 256), ceil_div(cb_height, 16), order);
}

static int sample(const Case &test, int plane, int x, int y) {
  const auto &data = test.constants;
  const int bytes = data.params.bit_depth == 8 ? 1 : 2;
  const auto *p = &test.pixels[data.src_planes[plane].y + y * data.src_planes[plane].x + x * bytes];
  return bytes == 1 ? p[0] : p[0] + 256 * p[1];
}

// AV1 7.18.3.5. Look up a sample in the current patch of the grain template and blend with
// its neighbors at a block boundary. The horizontal blends are clipped before the vertical blend.
static int reference_grain(const Case &test, int plane, int x, int y, int center) {
  const auto &data = test.constants;
  const int sub = plane != 0;
  const int block_size = 32 >> sub;
  const int bx = x / block_size, by = y / block_size;
  const int lx = x % block_size, ly = y % block_size;
  const int base = plane == 0 ? 0 : plane == 1 ? data.grain_offset_u : data.grain_offset_v;
  const int stride = sub ? data.chroma_grain_stride : data.luma_grain_stride;
  const auto patch = [&](int dx, int dy) {
    const int random = test.offsets[(by + dy) * data.random_offset_stride + bx + dx];
    const int u = data.left_pad + (2 * data.ar_padding >> sub) + (random / 16 << (1 - sub)) + lx - dx * block_size;
    const int v = data.top_pad + (2 * data.ar_padding >> sub) + (random % 16 << (1 - sub)) + ly - dy * block_size;
    return test.grain[base + v * stride + u];
  };
  const auto blend = [&](int previous, int current, int position) {
    const int prev_weight = sub ? 23 : position == 0 ? 27 : 17;
    const int cur_weight = sub ? 22 : position == 0 ? 17 : 27;
    return std::clamp((previous * prev_weight + current * cur_weight + 16) >> 5, -center, center - 1);
  };
  const bool overlap_x = data.params.overlap_flag && bx > 0 && lx < (2 >> sub);
  const auto horizontal = [&](int dy) {
    const int current = patch(0, dy);
    return overlap_x ? blend(patch(-1, dy), current, lx) : current;
  };
  const int value = horizontal(0);
  return data.params.overlap_flag && by > 0 && ly < (2 >> sub)
           ? blend(horizontal(-1), value, ly)
           : value;
}

static uint reference_pixel(const Case &test, int plane, int x, int y) {
  const auto &data = test.constants;
  const auto &p = data.params;
  const int shift = p.bit_depth - 8;
  const int center = 128 << shift;
  const int maximum = 2 * center - 1;
  const int output_shift = shift ? 6 : 0;
  if (plane && test.mode == Monochrome) return uint(center) << output_shift;
  const int input = sample(test, plane, x, y);
  const int points[] = {p.num_y_points, p.num_cb_points, p.num_cr_points};
  if (!points[plane] && !(plane && p.chroma_scaling_from_luma)) return uint(input) << output_shift;

  int index = input;
  if (plane) {
    const int left = x * 2, right = std::min(left + 1, data.width - 1);
    index = (sample(test, 0, left, y * 2) + sample(test, 0, right, y * 2) + 1) / 2;
    if (!p.chroma_scaling_from_luma) {
      const int luma_weight = (plane == 1 ? p.cb_luma_mult : p.cr_luma_mult) - 128;
      const int uv_weight = (plane == 1 ? p.cb_mult : p.cr_mult) - 128;
      const int offset = (plane == 1 ? p.cb_offset : p.cr_offset) - 256;
      index = std::clamp(((index * luma_weight + input * uv_weight) >> 6) + offset * (1 << shift), 0, maximum);
    }
  }

  const int step = 1 << shift;
  const int knot = index / step, remainder = index % step;
  const int a = data.scaling_lut[knot][plane], b = data.scaling_lut[std::min(knot + 1, 255)][plane];
  const int scale = (a * (step - remainder) + b * remainder + step / 2) / step;
  const int grain = reference_grain(test, plane, x, y, center);
  const int delta = (scale * grain + (1 << (p.scaling_shift - 1))) >> p.scaling_shift;
  const int low = p.clip_to_restricted_range ? 16 << shift : 0;
  const int high = p.clip_to_restricted_range ? (plane && !data.mc_identity ? 240 : 235) << shift : maximum;
  return uint(std::clamp(input + delta, low, high)) << output_shift;
}

static void reference_frame(const Case &test, Frame &frame) {
  for (int y = 0; y < frame.y.height; ++y)
    for (int x = 0; x < frame.y.width; ++x)
      frame.y.store(int2(x, y)) = reference_pixel(test, 0, std::min(x, test.constants.width - 1),
                                                    std::min(y, test.constants.height - 1));
  for (int y = 0; y < frame.uv.height; ++y)
    for (int x = 0; x < frame.uv.width; ++x)
      frame.uv.store(int2(x, y)) = uint2(reference_pixel(test, 1, x, y), reference_pixel(test, 2, x, y));
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

// Pack the extra 10-bit scale indices as the host does.
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
  reference_frame(test, expected);

  optimized::data = data;
  if (data.params.bit_depth == 10) pack_p010_scaling(optimized::data);
  optimized::src = {test.pixels.data(), test.pixels.size()};
  optimized::grain_block = {reinterpret_cast<const uint8_t *>(test.grain.data()), test.grain.size() * sizeof(int)};
  optimized::random_offset = {test.offsets.data(), test.offsets.size()};
  optimized::sp_luma = {&actual.y};
  optimized::sp_chroma = {&actual.uv};
  gs_state.shader = "film_grain_filter_sp_tex_v2";
  dispatch(optimized::shader_entry, uint3(optimized::kThreadsX, optimized::kThreadsY, optimized::kThreadsZ),
           ceil_div(data.width, 32), ceil_div(data.height, 32), order);
  if (!data.enable_chroma) {
    gs_state.shader = "copy_plane_sp_tex_v2";
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
  std::printf("PASS: %d NV12/P010 cases matched the scalar reference at every texel\n", count);
}
