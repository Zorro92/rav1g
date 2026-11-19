// SPDX-License-Identifier: BSD-2-Clause

#define GROUP_SCHED_IMPLEMENTATION
#include "hlsl.h"

#include <algorithm>
#include <array>
#include <cstdio>
#include <random>
#include <vector>

long g_out_of_bounds = 0;

template <int N>
vec<uint, N> operator<<(const vec<uint, N> &v, int shift) {
  return v << uint(shift);
}

template <class T>
struct Texture {
  uint width, height;
  std::vector<T> samples;
  std::vector<uint> writes;
  T discarded{};

  Texture(uint w, uint h) : width(w), height(h), samples(w * h), writes(w * h) {}

  T &store(int2 pos) {
    // Typed UAV stores outside the texture are discarded.
    if (pos.x < 0 || pos.y < 0 || uint(pos.x) >= width || uint(pos.y) >= height) return discarded;
    const uint i = uint(pos.y) * width + uint(pos.x);
    ++writes[i];
    return samples[i];
  }
};

template <class T>
struct RWTexture2D {
  Texture<T> *texture;
  T &operator[](int2 pos) const { return texture->store(pos); }
};

namespace legacy {
#define SP_TEXTURE
#include "copy_plane_sp.gen.h"
#include "copy_plane_sp.entry.h"
#undef SP_TEXTURE
#undef SP_LUMA_REGISTER
#undef SP_CHROMA_REGISTER
}

namespace tiled {
#include "copy_plane_sp_tex_v2.gen.h"
#include "copy_plane_sp_tex_v2.entry.h"
#undef SP_TEXTURE
#undef SP_LUMA_REGISTER
#undef SP_CHROMA_REGISTER
}

static uint ceil_div(uint n, uint d) { return (n + d - 1) / d; }

struct Input {
  uint width, height, bpp;
  std::array<uint, 3> offsets, strides;
  std::vector<uint8_t> bytes;

  Input(uint w, uint h, bool hbd, std::mt19937 &rng) : width(w), height(h), bpp(hbd ? 2 : 1) {
    // Use a nonzero base, bordered rows like the frame pool, and different plane contents.
    uint start = (1 + rng() % 64) * 16;
    for (uint plane = 0; plane < 3; ++plane) {
      const uint row_width = plane ? ceil_div(width, 2) : width;
      const uint rows = plane ? ceil_div(height, 2) : height;
      const uint stride = (ceil_div(row_width, 64) * 64 + 32) * bpp;
      strides[plane] = stride;
      offsets[plane] = start + 16 * bpp;
      bytes.resize(start + rows * stride);
      for (; start < bytes.size(); start += bpp) {
        const uint sample = rng() & (hbd ? 1023 : 255);
        bytes[start] = sample & 255;
        if (hbd) bytes[start + 1] = sample >> 8;
      }
    }
  }

  uint sample(uint plane, uint x, uint y) const {
    const uint at = offsets[plane] + y * strides[plane] + x * bpp;
    return bpp == 2 ? (uint(bytes[at]) | (uint(bytes[at + 1]) << 8)) << 6 : bytes[at];
  }
};

struct Output {
  Texture<uint> y;
  Texture<uint2> uv;
  Output(uint w, uint h) : y(2 * ceil_div(w, 2), 2 * ceil_div(h, 2)), uv(ceil_div(w, 2), ceil_div(h, 2)) {}
};

static void dispatch(void (*entry)(uint3, uint3), uint columns, uint rows, uint threads_x, uint threads_y) {
  for (uint gy = 0; gy < rows; ++gy)
    for (uint gx = 0; gx < columns; ++gx)
      for (uint ly = 0; ly < threads_y; ++ly)
        for (uint lx = 0; lx < threads_x; ++lx)
          entry(uint3(gx, gy, 0), uint3(lx, ly, 0));
}

static void run_legacy(Input &input, Output &out, bool luma, bool chroma, bool mono) {
  using namespace legacy;
  pool = {input.bytes.data(), input.bytes.size()};
  sp_luma = {&out.y};
  sp_chroma = {&out.uv};
  cb_width = input.width;
  cb_height = input.height;
  cb_hbd = input.bpp == 2;
  cb_src_offset_v = input.offsets[2];
  for (uint plane = 0; plane < 2; ++plane) {
    if (plane ? !chroma : !luma) continue;
    cb_src_offset = input.offsets[plane];
    cb_src_stride = input.strides[plane];
    cb_mode = plane ? (mono ? SP_MODE_CHROMA_NEUTRAL : SP_MODE_CHROMA) : SP_MODE_LUMA;
    cb_groups = ceil_div(plane ? out.uv.width : input.width, 8);
    cb_rows = plane ? out.uv.height : input.height;
    dispatch(shader_entry, ceil_div(cb_groups, kThreadsX), ceil_div(cb_rows, kThreadsY), kThreadsX, kThreadsY);
  }
}

static void run_tiled(Input &input, Output &out, bool luma, bool chroma, bool mono) {
  using namespace tiled;
  pool = {input.bytes.data(), input.bytes.size()};
  sp_luma = {&out.y};
  sp_chroma = {&out.uv};
  cb_width = input.width;
  cb_height = input.height;
  cb_hbd = input.bpp == 2;
  cb_y_offset = input.offsets[0];
  cb_u_offset = input.offsets[1];
  cb_v_offset = input.offsets[2];
  cb_y_stride = input.strides[0];
  cb_uv_stride = input.strides[1];
  cb_neutral_chroma = mono;
  // Match dispatch_copy_sp_tex in the host.
  cb_luma_group_rows = luma ? ceil_div(input.height, 4) : 0;
  const uint columns = chroma ? 2 * ceil_div(input.width, 256) : ceil_div(input.width, 128);
  const uint rows = cb_luma_group_rows + (chroma ? ceil_div(input.height, 16) : 0);
  dispatch(shader_entry, columns, rows, kThreadsX, kThreadsY);
}

static void mismatch(const Input &input, const char *plane, uint x, uint y) {
  std::fprintf(stderr, "MISMATCH %s at (%u,%u) for %ux%u %s\n",
               plane, x, y, input.width, input.height, input.bpp == 1 ? "NV12" : "P010");
  std::exit(1);
}

static void check(Input &input, bool luma, bool chroma, bool mono) {
  Output expected(input.width, input.height), actual(input.width, input.height);
  run_legacy(input, expected, luma, chroma, mono);
  run_tiled(input, actual, luma, chroma, mono);
  if (g_out_of_bounds) {
    std::fprintf(stderr, "copy_check: %ld out-of-bounds raw accesses\n", g_out_of_bounds);
    std::exit(2);
  }
  for (uint y = 0; y < actual.y.height; ++y) {
    for (uint x = 0; x < actual.y.width; ++x) {
      const uint i = y * actual.y.width + x;
      const uint oracle = luma ? input.sample(0, std::min(x, input.width - 1), std::min(y, input.height - 1)) : 0;
      if (expected.y.writes[i] != uint(luma) || actual.y.writes[i] != uint(luma) ||
          expected.y.samples[i] != oracle || actual.y.samples[i] != oracle) {
        mismatch(input, "Y", x, y);
      }
    }
  }
  const uint neutral = input.bpp == 2 ? 512u << 6 : 128u;
  for (uint y = 0; y < actual.uv.height; ++y) {
    for (uint x = 0; x < actual.uv.width; ++x) {
      const uint i = y * actual.uv.width + x;
      const uint u = chroma ? (mono ? neutral : input.sample(1, x, y)) : 0;
      const uint v = chroma ? (mono ? neutral : input.sample(2, x, y)) : 0;
      if (expected.uv.writes[i] != uint(chroma) || actual.uv.writes[i] != uint(chroma) ||
          expected.uv.samples[i].x != u || actual.uv.samples[i].x != u ||
          expected.uv.samples[i].y != v || actual.uv.samples[i].y != v) {
        mismatch(input, "UV", x, y);
      }
    }
  }
}

int main() {
  std::mt19937 rng(0x420c0f1);
  uint cases = 0;
  for (bool hbd : {false, true}) {
    for (uint w : {1, 3, 5, 7, 8, 127, 128, 129, 255, 256, 257}) {
      for (uint h : {1, 2, 3, 4, 7, 8, 9, 15, 16, 17}) {
        Input input(w, h, hbd, rng);
        check(input, true, true, false);
        ++cases;
      }
    }
    for (uint i = 0; i < 160; ++i) {
      const uint w = 1 + rng() % 541;
      const uint h = 1 + rng() % 133;
      Input input(w, h, hbd, rng);
      check(input, i % 3 != 0, i % 3 != 1, i % 2 == 0);
      ++cases;
    }
    Input input(3840, 2160, hbd, rng);
    check(input, true, true, false);
    ++cases;
  }
  std::printf("copy_check: %u NV12/P010 cases matched the legacy shaders and the pixel reference\n", cases);
}
