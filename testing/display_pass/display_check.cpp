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

// The tables of crates/rav1g/src/host/display_pass.rs, the same float32 functions.
namespace tables {
const int kEotfN = 1024, kEotfFine = 256, kGainN = 512, kWords = 1540;
const float kSplit = 1.0f / 32.0f, kPeak = 1000.0f, kWhite = 100.0f;

float pq_to_nits(float e) {
  const float p = powf(fminf(fmaxf(e, 0.0f), 1.0f), 1.0f / 78.84375f);
  return powf(fmaxf(p - 0.8359375f, 0.0f) / (18.8515625f - 18.6875f * p), 1.0f / 0.1593017578125f) * 10000.0f;
}
float nits_to_pq(float n) {
  const float p = powf(fminf(fmaxf(n, 0.0f), 10000.0f) / 10000.0f, 0.1593017578125f);
  return powf((0.8359375f + 18.8515625f * p) / (1.0f + 18.6875f * p), 78.84375f);
}
float eetf(float nits) {
  nits = fminf(fmaxf(nits, 0.0f), kPeak);
  const float target = fminf(kWhite * 2.03f, 10000.0f);
  if (kPeak <= target) return fminf(nits / target, 1.0f);
  const float peak = nits_to_pq(kPeak);
  const float s = nits_to_pq(nits) / peak;
  const float top = nits_to_pq(target) / peak;
  const float knee = 2.0f * top - 1.0f;
  float mapped = s;
  if (s >= knee) {
    const float t = (s - knee) / (1.0f - knee), t2 = t * t, t3 = t2 * t;
    mapped = (2.0f * t3 - 3.0f * t2 + 1.0f) * knee + (t3 - 2.0f * t2 + t) * (1.0f - knee) + (-2.0f * t3 + 3.0f * t2) * top;
  }
  return fminf(fmaxf(pq_to_nits(mapped * peak) / target, 0.0f), 1.0f);
}
float gain(float nits) { return eetf(nits) / fmaxf(nits, 1e-6f); }

// The words, and the constants eotf a, b, c, gain scale, offset.
std::vector<uint> make(uint mode, float constants[5]) {
  std::vector<uint> words(kWords, 0);
  const float a = kEotfFine / kSplit, b = (kEotfN - kEotfFine) / (1.0f - kSplit), c = kEotfFine - kSplit * b;
  const float target = fminf(kWhite * 2.03f, 10000.0f);
  const float pk = nits_to_pq(kPeak);
  const float start_e = kPeak <= target ? nits_to_pq(target) : (2.0f * (nits_to_pq(target) / pk) - 1.0f) * pk;
  const float start_nits = kPeak <= target ? target : pq_to_nits(start_e);
  float scale, offset;
  if (mode == 2) {
    const float u0 = log2f(start_nits);
    scale = kGainN / (10.0f - u0);
    for (int j = 0; j <= kGainN; j++) words[kEotfN + 1 + j] = asuint(gain(exp2f(u0 + j / scale)));
    offset = -u0 * scale;
  } else {
    scale = kGainN / (1.0f - start_e);
    for (int j = 0; j <= kGainN; j++) words[kEotfN + 1 + j] = asuint(gain(pq_to_nits(start_e + j / scale)));
    for (int k = 0; k <= kEotfN; k++) {
      const float e = k < kEotfFine ? k / a : (k - c) / b;
      words[k] = asuint(pq_to_nits(fminf(e, 1.0f)));
    }
    offset = -start_e * scale;
  }
  constants[0] = a;
  constants[1] = b;
  constants[2] = c;
  constants[3] = scale;
  constants[4] = offset;
  return words;
}
}  // namespace tables

struct Case {
  const char *name;
  uint sw, sh, ow, oh;
  uint transfer, matrix;  // H.273
  bool full, colocated, no_quad;
  int content;            // 0 natural-ish, 1 every code at random, 2 out of gamut just above black
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
  const uint y_stride = (ceil_div(c.sw, 64) * 64 + 32) * 2, uv_stride = (ceil_div(cw, 64) * 64 + 32) * 2;
  const uint y_off = 256, u_off = y_off + c.sh * y_stride + 64, v_off = u_off + ch * uv_stride + 128;
  std::vector<uint8_t> bytes((v_off + (ch - 1) * uv_stride + cw * 2 + 3) & ~3u, 0);
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
          v = (int)(rng() & 1023);
        } else if (c.content == 2) {
          const uint bx = p ? x / 4 : x / 8, by = p ? y / 4 : y / 8;
          v = crafted[3 * (by * bw + bx) + p];
        } else {
          // Gradients, blocks of flat colour and noise, with some samples out of the nominal range.
          const int block = (int)((x / 8 + y / 8 + p * 3) % 5);
          const int base = p == 0 ? (int)(64 + (x * 7 + y * 3) % 900) : (int)(512 + ((int)(x * 5) - (int)(y * 4)) % 400);
          v = block == 0 ? (p ? 512 : 64) : block == 1 ? (p ? 512 + 300 * (p == 1 ? 1 : -1) : 940) : base + noise(rng);
          if (rng() % 97 == 0) v = (int)(rng() & 1023);
        }
        v = v < 0 ? 0 : v > 1023 ? 1023 : v;
        planes[p][y * w + x] = (uint16_t)v;
        bytes[off + y * stride + x * 2] = (uint8_t)(v & 255);
        bytes[off + y * stride + x * 2 + 1] = (uint8_t)(v >> 8);
      }
    }
  }
  const uint mode = c.transfer == 16 ? 0u : c.transfer == 18 ? 2u : 1u;
  float k[5];
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
  cb_flags = mode | (c.matrix == 9 ? 4u : 0u) | (c.full ? 8u : 0u) | (c.colocated ? 0u : 16u) | (quad ? 32u : 0u);
  cb_eotf_a = k[0];
  cb_eotf_b = k[1];
  cb_eotf_c = k[2];
  cb_gain_scale = k[3];
  cb_gain_offset = k[4];
  cb_exact_below = 4e-4f;
  const bool wide = c.matrix == 9;
  const float wr = wide ? 0.2627f : 0.2126f, wb = wide ? 0.0593f : 0.0722f, wg = wide ? 0.6780f : 0.7152f;
  cb_ys = c.full ? 1.0f / 1023.0f : 1.0f / 876.0f;
  cb_yo = c.full ? 0.0f : -64.0f / 876.0f;
  cb_cs = c.full ? 1.0f / 1023.0f : 1.0f / 896.0f;
  cb_co = -512.0f * cb_cs;
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
  for (int p = 0; p < 3; p++) std::fwrite(planes[p].data(), 2, planes[p].size(), f);
  std::fclose(f);
  f = std::fopen(display.c_str(), "wb");
  std::vector<uint16_t> out(luma.size());
  for (size_t i = 0; i < luma.size(); i++) out[i] = (uint16_t)(luma[i] >> 6);
  std::fwrite(out.data(), 2, out.size(), f);
  for (int comp = 0; comp < 2; comp++) {
    std::vector<uint16_t> plane(chroma.size());
    for (size_t i = 0; i < chroma.size(); i++) plane[i] = (uint16_t)(chroma[i][comp] >> 6);
    std::fwrite(plane.data(), 2, plane.size(), f);
  }
  std::fclose(f);
  std::printf("%s %s %s --size %ux%u --out %ux%u --transfer %u --matrix %u%s%s\n", c.name, frames.c_str(),
              display.c_str(), c.sw, c.sh, c.ow, c.oh, c.transfer, c.matrix, c.full ? " --full" : "",
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
  };
  std::mt19937 rng(0x5eed);
  int failed = 0;
  for (const Case &c : cases) failed |= run_case(c, argv[1], rng);
  return failed;
}
