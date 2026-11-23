// SPDX-License-Identifier: BSD-2-Clause
//
// display_pass.hlsl compiled as C++ (testing/sync_check/to_cpp.py) and run on test frames, with the
// thread groups of testing/common/group_sched.h: every thread of a group alone between two group syncs,
// in an order drawn anew at every sync, and group memory that records every read of what another thread
// wrote since the last sync. For each case the program writes the frame (planar, 16-bit samples, as
// rav1g-dec -o writes them) and the display image (the same layout at the output size, 10-bit codes) to
// the output directory and a line of model.py arguments to stdout; run.sh holds them against the model.
// It fails itself on a race, a sync error, a read outside the frame pool, or an output sample not
// written exactly once.
//
//   display_check <out dir>

#define GROUP_SCHED_IMPLEMENTATION
#include "hlsl_float.h"

#include <cstdio>
#include <cstdlib>
#include <random>
#include <string>
#include <vector>

long g_out_of_bounds = 0;

// What a group finds in its group memory is not defined: garbage before every group.
static void scramble_bytes(void *p, size_t size, uint64_t *state) {
  uint8_t *bytes = (uint8_t *)p;
  for (size_t i = 0; i + 4 <= size; i += 4) {
    *state = *state * 6364136223846793005ull + 1442695040888963407ull;
    const uint32_t v = (uint32_t)(*state >> 32);
    memcpy(bytes + i, &v, 4);
  }
}

namespace shader {
#include "display_pass.gen.h"
#include "display_pass.entry.h"
}  // namespace shader

// The tables of crates/rav1g/src/host/display_pass.rs: the same float64 functions, nodes and cubics.
namespace tables {
const int kEotfN = 256, kEotfFine = 64, kGainN = 96, kGainLow = 64, kWords = 4 * (kEotfN + 1 + kGainN + 1);
const double kSplit = 1.0 / 32.0, kPeak = 1000.0, kWhite = 100.0;
const double M1 = 2610.0 / 16384.0, M2 = 2523.0 / 32.0, C1 = 3424.0 / 4096.0, C2 = 2413.0 / 128.0, C3 = 2392.0 / 128.0;

double pq_to_nits(double e) {
  const double p = pow(fmin(fmax(e, 0.0), 1.0), 1.0 / M2);
  return pow(fmax(p - C1, 0.0) / (C2 - C3 * p), 1.0 / M1) * 10000.0;
}
double nits_to_pq(double n) {
  const double p = pow(fmin(fmax(n, 0.0), 10000.0) / 10000.0, M1);
  return pow((C1 + C2 * p) / (1.0 + C3 * p), M2);
}
double target() { return fmin(kWhite * 2.03, 10000.0); }
double eetf(double nits) {
  nits = fmin(fmax(nits, 0.0), kPeak);
  if (kPeak <= target()) return fmin(nits / target(), 1.0);
  const double peak = nits_to_pq(kPeak), s = nits_to_pq(nits) / peak, top = nits_to_pq(target()) / peak;
  const double knee = 2.0 * top - 1.0;
  double mapped = s;
  if (s >= knee) {
    const double t = (s - knee) / (1.0 - knee), t2 = t * t, t3 = t2 * t;
    mapped = (2.0 * t3 - 3.0 * t2 + 1.0) * knee + (t3 - 2.0 * t2 + t) * (1.0 - knee) + (-2.0 * t3 + 3.0 * t2) * top;
  }
  return fmin(fmax(pq_to_nits(mapped * peak) / target(), 0.0), 1.0);
}
double gain(double nits) { return eetf(nits) / fmax(nits, 1e-6); }

template <class X, class Fn>
void fill(uint *words, int n, X x, Fn f) {
  for (int k = 0; k < n; k++) {
    const double x0 = x(k), x1 = x(k + 1), h = x1 - x0, e = h * 1e-4, v0 = f(x0), v1 = f(x1);
    const double d0 = (-3.0 * v0 + 4.0 * f(x0 + e) - f(x0 + 2.0 * e)) / (2.0 * e) * h;
    const double d1 = (3.0 * v1 - 4.0 * f(x1 - e) + f(x1 - 2.0 * e)) / (2.0 * e) * h;
    words[4 * k] = asuint((float)v0);
    words[4 * k + 1] = asuint((float)d0);
    words[4 * k + 2] = asuint((float)(3.0 * (v1 - v0) - 2.0 * d0 - d1));
    words[4 * k + 3] = asuint((float)(2.0 * (v0 - v1) + d0 + d1));
  }
  words[4 * n] = asuint((float)f(x(n)));
}

// The words, and the constants eotf a, b, c and the gain's s1, o1, s2, o2.
std::vector<uint> make(uint mode, float constants[7]) {
  std::vector<uint> words(kWords, 0);
  const double a = kEotfFine / kSplit, b = (kEotfN - kEotfFine) / (1.0 - kSplit), c = kEotfFine - kSplit * b;
  const double pk = nits_to_pq(kPeak);
  const double knee = kPeak <= target() ? nits_to_pq(target()) : (2.0 * (nits_to_pq(target()) / pk) - 1.0) * pk;
  const double peak = kPeak <= target() ? knee : pk;
  uint *gain_words = words.data() + 4 * (kEotfN + 1);
  double lines[4];
  if (mode == 2) {
    const double u0 = log2(pq_to_nits(knee)), step = (log2(kPeak) - u0) / (kGainN - 1);
    fill(gain_words, kGainN, [&](int j) { return u0 + j * step; }, [](double u) { return gain(exp2(u)); });
    lines[0] = lines[2] = 1.0 / step;
    lines[1] = lines[3] = -u0 / step;
  } else {
    const double s1 = kGainLow / (peak - knee), s2 = (kGainN - kGainLow) / (1.0 - peak);
    fill(gain_words, kGainN, [&](int j) { return j < kGainLow ? knee + j / s1 : peak + (j - kGainLow) / s2; },
         [](double e) { return gain(pq_to_nits(e)); });
    fill(words.data(), kEotfN, [&](int k) { return k < kEotfFine ? k / a : (k - c) / b; },
         [](double e) { return pq_to_nits(e); });
    lines[0] = s1;
    lines[1] = -knee * s1;
    lines[2] = s2;
    lines[3] = kGainLow - peak * s2;
  }
  constants[0] = (float)a;
  constants[1] = (float)b;
  constants[2] = (float)c;
  for (int i = 0; i < 4; i++) constants[3 + i] = (float)lines[i];
  return words;
}
}  // namespace tables

struct Case {
  const char *name;
  uint sw, sh, ow, oh;
  uint transfer, matrix;  // H.273
  bool full, colocated, no_quad;
  int content;            // 0 natural-ish, 1 every code at random, 2 out of gamut just above black
  int depth = 10;         // 8: a byte a sample, NV12 codes
};

static uint ceil_div(uint n, uint d) { return (n + d - 1) / d; }
static uint64_t g_order = 0x9e3779b97f4a7c15ull;

static void thread_body(int thread, void *context) {
  const uint3 group = *static_cast<uint3 *>(context);
  shader::shader_entry(group, uint3((uint)thread, 0, 0));
}

static int run_case(const Case &c, const std::string &dir, std::mt19937 &rng) {
  using namespace shader;
  // The frame pool: a base, three planes of 16-bit samples with padded rows, V last and the pool ending
  // with the dword of its last visible sample, so that a read past it is seen.
  const uint cw = ceil_div(c.sw, 2), ch = ceil_div(c.sh, 2);
  const uint bpp = c.depth == 8 ? 1 : 2, top = c.depth == 8 ? 255 : 1023;
  const uint y_stride = (ceil_div(c.sw, 64) * 64 + 32) * bpp, uv_stride = (ceil_div(cw, 64) * 64 + 32) * bpp;
  const uint y_off = 256, u_off = y_off + c.sh * y_stride + 64, v_off = u_off + ch * uv_stride + 128;
  std::vector<uint8_t> bytes((v_off + (ch - 1) * uv_stride + cw * bpp + 3) & ~3u, 0);
  std::vector<uint16_t> planes[3] = { std::vector<uint16_t>(c.sw * c.sh), std::vector<uint16_t>(cw * ch),
                                      std::vector<uint16_t>(cw * ch) };
  std::uniform_int_distribution<int> noise(-24, 24);
  // Content 2: flat 8x8 blocks (4x4 chroma) of pixels below black whose largest R'G'B' signal is just
  // above 0 (BT.2020, limited range) and whose other channels are far out of gamut: their tone-map gain
  // is eetf(peak) / 1e-6 with a peak below 1e-6 nits, which the shader computes exactly.
  const uint bw = ceil_div(c.sw, 8), bh = ceil_div(c.sh, 8);
  std::vector<int> crafted(bw * bh * 3);
  for (uint i = 0; i < bw * bh; i++) {
    const int y = (int)(rng() % 64), u = (int)(rng() % 1024);
    int best_v = 512;
    float best = 1e9f;
    for (int v = 0; v < 1024; v++) {
      const float yy = (y - 64) / 876.0f, uu = (u - 512) / 896.0f, vv = (v - 512) / 896.0f;
      const float r = yy + 1.4746f * vv, b = yy + 1.8814f * uu, g = (yy - 0.2627f * r - 0.0593f * b) / 0.6780f;
      const float m = fmaxf(r, fmaxf(g, b));
      const float target = (float)(i % 7) * 4e-5f + 2e-5f;
      if (m > 0.0f && fabsf(m - target) < best) {
        best = fabsf(m - target);
        best_v = v;
      }
    }
    crafted[3 * i] = y;
    crafted[3 * i + 1] = u;
    crafted[3 * i + 2] = best_v;
  }
  for (int p = 0; p < 3; p++) {
    const uint w = p ? cw : c.sw, h = p ? ch : c.sh;
    const uint off = p == 0 ? y_off : p == 1 ? u_off : v_off, stride = p ? uv_stride : y_stride;
    for (uint y = 0; y < h; y++) {
      for (uint x = 0; x < w; x++) {
        int v;
        if (c.content == 1) {
          v = (int)(rng() & top);
        } else if (c.content == 2) {
          const uint bx = p ? x / 4 : x / 8, by = p ? y / 4 : y / 8;
          v = crafted[3 * (by * bw + bx) + p];
        } else {
          // Gradients, blocks of flat colour and noise, with some samples out of the nominal range.
          const int block = (int)((x / 8 + y / 8 + p * 3) % 5);
          const int base = p == 0 ? (int)(64 + (x * 7 + y * 3) % 900) : (int)(512 + ((int)(x * 5) - (int)(y * 4)) % 400);
          v = block == 0 ? (p ? 512 : 64) : block == 1 ? (p ? 512 + 300 * (p == 1 ? 1 : -1) : 940) : base + noise(rng);
          if (rng() % 97 == 0) v = (int)(rng() & 1023);
          if (c.depth == 8) v >>= 2;
        }
        v = v < 0 ? 0 : v > (int)top ? (int)top : v;
        planes[p][y * w + x] = (uint16_t)v;
        bytes[off + y * stride + x * bpp] = (uint8_t)(v & 255);
        if (bpp == 2) bytes[off + y * stride + x * 2 + 1] = (uint8_t)(v >> 8);
      }
    }
  }
  const uint mode = c.transfer == 16 ? 0u : c.transfer == 18 ? 2u : 1u;
  float k[7];
  std::vector<uint> lut = tables::make(mode, k);
  pool = { bytes.data(), bytes.size() };
  lut_src = { reinterpret_cast<const uint8_t *>(lut.data()), lut.size() * 4 };
  cb_src_w = c.sw;
  cb_src_h = c.sh;
  cb_out_w = c.ow;
  cb_out_h = c.oh;
  cb_y_offset = y_off;
  cb_u_offset = u_off;
  cb_v_offset = v_off;
  cb_y_stride = y_stride;
  cb_uv_stride = uv_stride;
  const bool quad = !c.no_quad && c.sw == 2 * c.ow && c.sh == 2 * c.oh;
  cb_flags = mode | (c.matrix == 9 ? 4u : 0u) | (c.full ? 8u : 0u) | (c.colocated ? 0u : 16u) | (quad ? 32u : 0u) |
             (c.depth == 8 ? 64u : 0u);
  cb_eotf_a = k[0];
  cb_eotf_b = k[1];
  cb_eotf_c = k[2];
  cb_gain_s1 = k[3];
  cb_gain_o1 = k[4];
  cb_gain_s2 = k[5];
  cb_gain_o2 = k[6];
  cb_exact_below = 4e-4f;
  const bool wide = c.matrix == 9;
  const float wr = wide ? 0.2627f : 0.2126f, wb = wide ? 0.0593f : 0.0722f, wg = wide ? 0.6780f : 0.7152f;
  const float black = c.depth == 8 ? 16.0f : 64.0f, yr = c.depth == 8 ? 219.0f : 876.0f;
  const float mid = c.depth == 8 ? 128.0f : 512.0f, cr = c.depth == 8 ? 224.0f : 896.0f, full = (float)top;
  cb_ys = 1.0f / (c.full ? full : yr);
  cb_yo = c.full ? 0.0f : -black / yr;
  cb_cs = 1.0f / (c.full ? full : cr);
  cb_co = -mid * cb_cs;
  cb_kr = 2.0f * (1.0f - wr);
  cb_kb = 2.0f * (1.0f - wb);
  cb_kgu = -wb * cb_kb / wg;
  cb_kgv = -wr * cb_kr / wg;

  std::vector<uint> luma(c.ow * c.oh), luma_writes(c.ow * c.oh);
  std::vector<uint2> chroma((c.ow / 2) * (c.oh / 2));
  std::vector<uint> chroma_writes(chroma.size());
  Texture<uint> ty;
  ty.width = c.ow;
  ty.height = c.oh;
  ty.samples = luma.data();
  ty.writes = luma_writes.data();
  Texture<uint2> tc;
  tc.width = c.ow / 2;
  tc.height = c.oh / 2;
  tc.samples = chroma.data();
  tc.writes = chroma_writes.data();
  sp_luma = { &ty };
  sp_chroma = { &tc };

  g_out_of_bounds = 0;
  gs_reset_report();
  gs_state.shader = "display_pass.hlsl";
  gs_state.track_unwritten = true;
  uint64_t garbage = 0x1234567;
  for (uint gy = 0; gy < ceil_div(c.oh, 64); gy++) {
    for (uint gx = 0; gx < ceil_div(c.ow, 64); gx++) {
      shader_scramble(&garbage);
      uint3 group(gx, gy, 0);
      gs_run_group(kThreadsX, thread_body, &group, &g_order);
    }
  }
  gs_state.track_unwritten = false;
  int failed = 0;
  if (g_out_of_bounds || gs_state.races || gs_state.sync_errors || gs_state.plain_barriers || gs_state.out_of_bounds ||
      gs_state.runaway_groups) {
    std::fprintf(stderr, "display_check: %s: %ld reads outside the pool, %ld races, %ld sync errors\n", c.name,
                 g_out_of_bounds, (long)gs_state.races, (long)gs_state.sync_errors);
    gs_print_races("  ");
    failed = 1;
  }
  for (size_t i = 0; i < luma_writes.size(); i++) failed |= luma_writes[i] != 1;
  for (size_t i = 0; i < chroma_writes.size(); i++) failed |= chroma_writes[i] != 1;
  if (failed) {
    std::fprintf(stderr, "display_check: %s: an output sample not written exactly once, or the run above\n", c.name);
    return 1;
  }

  const std::string frames = dir + "/" + c.name + ".frames.yuv", display = dir + "/" + c.name + ".display.yuv";
  FILE *f = std::fopen(frames.c_str(), "wb");
  // As rav1g-dec -o: 16-bit little-endian samples, a byte at 8 bits; P010's codes from their high bits.
  auto put = [&](FILE *to, uint v) {
    if (bpp == 2) {
      const uint16_t s16 = (uint16_t)v;
      std::fwrite(&s16, 2, 1, to);
    } else {
      const uint8_t s8 = (uint8_t)v;
      std::fwrite(&s8, 1, 1, to);
    }
  };
  for (int p = 0; p < 3; p++)
    for (uint16_t v : planes[p]) put(f, v);
  std::fclose(f);
  f = std::fopen(display.c_str(), "wb");
  const uint shift = c.depth == 8 ? 0 : 6;
  for (size_t i = 0; i < luma.size(); i++) put(f, luma[i] >> shift);
  for (int comp = 0; comp < 2; comp++)
    for (size_t i = 0; i < chroma.size(); i++) put(f, chroma[i][comp] >> shift);
  std::fclose(f);
  std::printf("%s %s %s --size %ux%u --out %ux%u --transfer %u --matrix %u --depth %d%s%s\n", c.name, frames.c_str(),
              display.c_str(), c.sw, c.sh, c.ow, c.oh, c.transfer, c.matrix, c.depth, c.full ? " --full" : "",
              c.colocated ? " --colocated" : "");
  return 0;
}

int main(int argc, char **argv) {
  if (argc != 2) {
    std::fprintf(stderr, "usage: display_check <out dir>\n");
    return 2;
  }
  const Case cases[] = {
    // 2:1, the quad path: PQ BT.2020 (HDR10), SDR BT.709, SDR BT.2020, HLG, a partial tile, full range,
    // colocated chroma, every code at random.
    { "pq-2to1", 256, 192, 128, 96, 16, 9, false, false, false, 0 },
    { "sdr709-2to1", 256, 128, 128, 64, 1, 1, false, false, false, 0 },
    { "sdr2020-2to1", 200, 140, 100, 70, 14, 9, false, false, false, 0 },
    { "hlg-2to1", 256, 136, 128, 68, 18, 9, false, false, false, 0 },
    { "pq-full-colocated-2to1", 136, 264, 68, 132, 16, 9, true, true, false, 0 },
    { "pq-codes-2to1", 128, 128, 64, 64, 16, 9, false, false, false, 1 },
    { "pq-black-2to1", 128, 128, 64, 64, 16, 9, false, false, false, 2 },
    { "sdr709-codes-2to1", 128, 128, 64, 64, 1, 1, false, false, false, 1 },
    // The area path: 2:1 without the quad path, 1.5:1, 1:1, upscaling, odd source sizes.
    { "pq-2to1-area", 192, 128, 96, 64, 16, 9, false, false, true, 0 },
    { "pq-3to2", 240, 132, 160, 88, 16, 9, false, false, false, 0 },
    { "sdr709-1to1", 130, 70, 130, 70, 1, 1, false, false, false, 0 },
    { "hlg-up", 96, 54, 160, 90, 18, 9, false, true, false, 0 },
    { "pq-odd", 201, 99, 120, 60, 16, 9, false, false, false, 1 },
    // 8-bit frames, NV12 codes: the quad path (aligned 2:1, a partial tile), full range, the area path.
    { "sdr709-8bit-2to1", 264, 136, 132, 68, 1, 1, false, false, false, 0, 8 },
    { "sdr709-8bit-codes-2to1", 128, 128, 64, 64, 1, 1, false, false, false, 1, 8 },
    { "pq-8bit-full-2to1", 128, 64, 64, 32, 16, 9, true, true, false, 0, 8 },
    { "sdr709-8bit-3to2", 210, 120, 140, 80, 1, 1, false, false, false, 0, 8 },
  };
  std::mt19937 rng(0x5eed);
  int failed = 0;
  for (const Case &c : cases) failed |= run_case(c, argv[1], rng);
  return failed;
}
