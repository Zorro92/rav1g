/*
 * Check of upscaling.hlsl (super-resolution) against libaom's normative upscale.
 *
 * The shader is compiled as C++ (testing/sync_check/to_cpp.py and hlsl.h) and run thread by thread over
 * planes laid out like the decoder's frame buffers, with the constants av1_superres() in
 * libav1/dx/loopfilters.cpp gives it. The reference is av1_upscale_normative_rows() of libaom's C code in
 * this tree (ref_glue.c), which is the upscaling process of the AV1 specification ("Upscaling process" in
 * the decoding process chapter): the step and the initial subpixel position from the two plane widths, eight
 * taps, source columns clamped to 0 and to the last column of the last mode info unit.
 *
 * Every visible sample of the upscaled plane must be equal, and the shader may write nothing but the rows
 * of the destination plane.
 *
 * Runs on a Linux host, no GPU:  sh testing/superres_check/run.sh
 */
#define GROUP_SCHED_IMPLEMENTATION
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include <vector>

#include "hlsl.h"
#include "ref_glue.h"

long g_out_of_bounds = 0;

namespace shader {
static void scramble_bytes(void *, size_t, uint64_t *) {}
#include "upscaling.gen.h"
#include "upscaling.entry.h"
}  // namespace shader

struct Rng {
  uint64_t state;
  uint32_t u32() {
    state += 0x9e3779b97f4a7c15ull;
    uint64_t z = state;
    z = (z ^ (z >> 30)) * 0xbf58476d1ce4e5b9ull;
    z = (z ^ (z >> 27)) * 0x94d049bb133111ebull;
    return (uint32_t)((z ^ (z >> 31)) >> 16);
  }
  int below(int n) { return (int)(u32() % (uint32_t)n); }
};

struct Case {
  int upscaled_width;
  int denominator;
  int height;
  int hbd;        // 16-bit samples, 10-bit values
  int content;    // 0 noise, 1 extremes, 2 ramp, 3 flat with spikes
  int sb_log2;
  int tile_cols;  // 0: uniform as many as fit
};

static int g_verbose = 0;
static long g_samples = 0;

// Runs one case: all three planes. Returns the number of failures (0 or 1).
static int run_case(const Case &c, Rng *rng, int index) {
  const int bytes = c.hbd ? 2 : 1;
  const int max_value = c.hbd ? 1023 : 255;
  const int width = ref_downscaled_width(c.upscaled_width, c.denominator);
  const int width8 = (width + 7) & ~7;
  const int border = 16;
  // The decoder's layout (av1_reallocate_frame_buffer, av1_cdef_looprestoration): strides of the 128-aligned
  // width plus two borders, the source planes at the coded width, the destination at the upscaled one.
  const int rows[3] = {c.height, (c.height + 1) >> 1, (c.height + 1) >> 1};
  const int src_stride_y = ((width + 127) & ~127) + 2 * border;
  const int dst_stride_y = ((c.upscaled_width + 127) & ~127) + 2 * border;
  const int src_strides[3] = {src_stride_y * bytes, ((((width + 127) & ~127) >> 1) + 2 * border) * bytes,
                              ((((width + 127) & ~127) >> 1) + 2 * border) * bytes};
  const int dst_strides[3] = {dst_stride_y * bytes, ((((c.upscaled_width + 127) & ~127) >> 1) + 2 * border) * bytes,
                              ((((c.upscaled_width + 127) & ~127) >> 1) + 2 * border) * bytes};
  const int rows_alloc = (c.height + 7) & ~7;
  size_t at = 256;
  int src_offsets[3], dst_offsets[3];
  for (int p = 0; p < 3; ++p) {
    src_offsets[p] = (int)at + border * bytes;
    at += (size_t)src_strides[p] * (p ? rows_alloc >> 1 : rows_alloc);
  }
  at = (at + 255) & ~(size_t)255;
  const size_t dst_start = at;
  for (int p = 0; p < 3; ++p) {
    dst_offsets[p] = (int)at + border * bytes;
    at += (size_t)dst_strides[p] * (p ? rows_alloc >> 1 : rows_alloc);
  }
  const size_t pool_size = (at + 255) & ~(size_t)255;
  std::vector<uint8_t> pool(pool_size);
  // Everything is noise first, the borders and the destination included: nothing but the source rows may
  // matter, and what the shader does not write must stay.
  for (size_t i = 0; i < pool_size; ++i) pool[i] = (uint8_t)rng->below(256);

  const int plane_w[3] = {width, (width + 1) >> 1, (width + 1) >> 1};
  const int plane_w8[3] = {width8, width8 >> 1, width8 >> 1};
  const int up_w[3] = {c.upscaled_width, (c.upscaled_width + 1) >> 1, (c.upscaled_width + 1) >> 1};
  for (int p = 0; p < 3; ++p) {
    // The source samples, up to the last column of the last mode info unit: decoded samples, any value.
    for (int y = 0; y < rows[p]; ++y) {
      for (int x = 0; x < plane_w8[p]; ++x) {
        int v;
        switch (c.content) {
          case 0: v = rng->below(max_value + 1); break;
          case 1: v = rng->below(2) ? max_value : 0; break;
          case 2: v = (x * 7 + y * 3) & max_value; break;
          default: v = rng->below(16) ? (max_value >> 1) : rng->below(max_value + 1); break;
        }
        uint8_t *s = &pool[(size_t)src_offsets[p] + (size_t)y * src_strides[p] + (size_t)x * bytes];
        s[0] = (uint8_t)(v & 0xff);
        if (c.hbd) s[1] = (uint8_t)(v >> 8);
      }
    }
  }
  const std::vector<uint8_t> before = pool;

  RefUpscale u = {};
  u.upscaled_width = c.upscaled_width;
  u.denominator = c.denominator;
  u.bit_depth = c.hbd ? 10 : 8;
  u.use_highbitdepth = c.hbd;
  u.sb_size_log2 = c.sb_log2;
  const int sb_cols = (width8 + (1 << c.sb_log2) - 1) >> c.sb_log2;
  u.tile_cols = c.tile_cols > 0 ? (c.tile_cols < sb_cols ? c.tile_cols : sb_cols) : 1;
  if (u.tile_cols > 64) u.tile_cols = 64;
  // Tile columns of random widths, each at least one superblock.
  {
    std::vector<int> cut(sb_cols + 1, 0);
    cut[0] = cut[sb_cols] = 1;
    for (int n = 1; n < u.tile_cols;) {
      const int k = 1 + rng->below(sb_cols - 1);
      if (!cut[k]) {
        cut[k] = 1;
        ++n;
      }
    }
    int n = 0;
    for (int k = 0; k <= sb_cols; ++k)
      if (cut[k]) u.tile_col_start_sb[n++] = k;
  }

  int failed = 0;
  for (int p = 0; p < 3 && !failed; ++p) {
    // The reference, from a copy of the source rows with the border libaom reads on the left.
    const int ref_src_stride = plane_w8[p] + 64;
    const int ref_dst_stride = up_w[p] + 64;
    std::vector<uint16_t> ref_src16((size_t)ref_src_stride * rows[p] + 64, 0x5555);
    std::vector<uint16_t> ref_dst16((size_t)ref_dst_stride * rows[p] + 64, 0x5555);
    std::vector<uint8_t> ref_src8((size_t)ref_src_stride * rows[p] + 64, 0x55);
    std::vector<uint8_t> ref_dst8((size_t)ref_dst_stride * rows[p] + 64, 0x55);
    for (int y = 0; y < rows[p]; ++y) {
      for (int x = 0; x < plane_w8[p]; ++x) {
        const uint8_t *s = &before[(size_t)src_offsets[p] + (size_t)y * src_strides[p] + (size_t)x * bytes];
        const int v = c.hbd ? (s[0] | (s[1] << 8)) : s[0];
        ref_src16[(size_t)y * ref_src_stride + 32 + x] = (uint16_t)v;
        ref_src8[(size_t)y * ref_src_stride + 32 + x] = (uint8_t)v;
      }
    }
    u.plane = p;
    u.rows = rows[p];
    if (c.hbd)
      ref_upscale_plane(&u, &ref_src16[32], ref_src_stride, &ref_dst16[32], ref_dst_stride);
    else
      ref_upscale_plane(&u, &ref_src8[32], ref_src_stride, &ref_dst8[32], ref_dst_stride);

    // The shader, with the constants of av1_superres().
    shader::dst_frame.ptr = pool.data();
    shader::dst_frame.size = pool.size();
    for (int q = 0; q < 3; ++q) {
      shader::src_planes.data[q] = uint4((uint)src_strides[q], (uint)src_offsets[q], (uint)plane_w[q], (uint)plane_w8[q]);
      shader::dst_planes.data[q] = uint4((uint)dst_strides[q], (uint)dst_offsets[q], (uint)up_w[q], 0);
    }
    shader::cb_plane = (uint)p;
    shader::cb_wi_count = (uint)(rows[p] * ((up_w[p] + 3) >> 2));
    shader::cb_hbd = (uint)c.hbd;
    const int groups = ((int)shader::cb_wi_count + 63) >> 6;
    for (int g = 0; g < groups; ++g)
      for (int t = 0; t < 64; ++t) shader::shader_entry(uint3((uint)g, 0, 0), uint3((uint)t, 0, 0));

    for (int y = 0; y < rows[p] && !failed; ++y) {
      for (int x = 0; x < up_w[p]; ++x) {
        const uint8_t *d = &pool[(size_t)dst_offsets[p] + (size_t)y * dst_strides[p] + (size_t)x * bytes];
        const int got = c.hbd ? (d[0] | (d[1] << 8)) : d[0];
        const int want = c.hbd ? ref_dst16[(size_t)y * ref_dst_stride + 32 + x] : ref_dst8[(size_t)y * ref_dst_stride + 32 + x];
        ++g_samples;
        if (got != want) {
          printf("MISMATCH case %d: upscaled width %d, denominator %d (coded width %d), %s, plane %d, %d tile "
                 "columns: sample %d,%d is %d, libaom has %d\n",
                 index, c.upscaled_width, c.denominator, width, c.hbd ? "10-bit" : "8-bit", p, u.tile_cols, x, y, got,
                 want);
          failed = 1;
          break;
        }
      }
    }
  }
  // Outside the rows of the destination planes nothing may have changed: not the source, not the borders
  // of the destination rows beyond the word the last visible sample is in, not the rows below.
  for (size_t i = 0; i < pool_size && !failed; ++i) {
    if (pool[i] == before[i]) continue;
    bool allowed = false;
    for (int p = 0; p < 3 && !allowed; ++p) {
      const long rel = (long)i - dst_offsets[p];
      if (rel < 0) continue;
      const long y = rel / dst_strides[p];
      const long xb = rel % dst_strides[p];
      allowed = y < rows[p] && xb < (long)((up_w[p] + 3) & ~3) * bytes;
    }
    if (!allowed) {
      printf("MISMATCH case %d: upscaled width %d, denominator %d, %s: byte %zu of the pool was written, which is "
             "%s\n",
             index, c.upscaled_width, c.denominator, c.hbd ? "10-bit" : "8-bit", i,
             i < dst_start ? "in the source planes" : "outside the rows of the destination planes");
      failed = 1;
    }
  }
  if (g_out_of_bounds) {
    printf("MISMATCH case %d: %ld accesses outside the frame buffer pool\n", index, g_out_of_bounds);
    g_out_of_bounds = 0;
    failed = 1;
  }
  if (g_verbose)
    printf("case %d: upscaled width %d, denominator %d (coded width %d), height %d, %s, content %d, %d tile columns: %s\n",
           index, c.upscaled_width, c.denominator, width, c.height, c.hbd ? "10-bit" : "8-bit", c.content, u.tile_cols,
           failed ? "FAILED" : "ok");
  return failed;
}

int main(int argc, char **argv) {
  int cases = 600;
  uint64_t seed = 20261112;
  for (int i = 1; i < argc; ++i) {
    if (!strcmp(argv[i], "--cases") && i + 1 < argc) {
      cases = atoi(argv[++i]);
    } else if (!strcmp(argv[i], "--seed") && i + 1 < argc) {
      seed = strtoull(argv[++i], NULL, 10);
    } else if (!strcmp(argv[i], "-v")) {
      g_verbose = 1;
    } else {
      fprintf(stderr, "usage: superres_check [--cases N] [--seed S] [-v]\n");
      return 2;
    }
  }
  Rng rng = {seed};
  int failures = 0;
  int run = 0;
  // Every denominator at a few fixed widths (among them widths that are not a multiple of 8 or of 4 at
  // either size, and 1920: the size of the conformance stream), 8-bit and 10-bit.
  static const int kWidths[] = {1920, 1280, 640, 352, 351, 350, 349, 226, 130, 66, 34, 17};
  for (size_t w = 0; w < sizeof(kWidths) / sizeof(kWidths[0]); ++w) {
    for (int denom = 9; denom <= 16; ++denom) {
      for (int hbd = 0; hbd < 2; ++hbd) {
        Case c = {kWidths[w], denom, kWidths[w] >= 1280 ? 6 : 18, hbd, (int)((w + denom + hbd) % 4), 6, 1 + (int)(w % 3)};
        failures += run_case(c, &rng, run++);
      }
    }
  }
  // Random sizes.
  for (int i = 0; i < cases; ++i) {
    Case c;
    c.upscaled_width = rng.below(8) ? 16 + rng.below(700) : 700 + rng.below(3400);
    c.denominator = 9 + rng.below(8);
    c.height = 1 + rng.below(c.upscaled_width > 700 ? 6 : 40);
    c.hbd = rng.below(2);
    c.content = rng.below(4);
    c.sb_log2 = 6 + rng.below(2);
    c.tile_cols = rng.below(3) ? 1 : 1 + rng.below(8);
    failures += run_case(c, &rng, run++);
    if (failures > 20) break;
  }
  printf("superres_check: %d cases, %ld samples compared with libaom, %d failed\n", run, g_samples, failures);
  return failures ? 1 : 0;
}
