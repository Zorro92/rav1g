/*
 * Self-test of the comparisons of the Shader Model 6 test (sm6test_checks.h), on a Linux host, no GPU.
 *
 * The test shaders are emulated here, written from the HLSL and not from the comparison code: thread by
 * thread, with the waves a device could form (4 to 64 lanes, lanes in thread order or reversed, waves that
 * are not full). Every emulated output must pass, and every output with one word changed must fail: a
 * comparison that passes anything says nothing about a device.
 *
 *   sh testing/sm6test/run.sh
 */

#include <stdio.h>
#include <stdlib.h>

#include "sm6test/sm6test_checks.h"

using namespace sm6check;

namespace {

int g_failures = 0;
int g_checks = 0;

void Expect(bool ok, const std::string& what, const Verdict& v) {
  ++g_checks;
  if (ok) return;
  ++g_failures;
  printf("FAILED: %s\n  details: %s\n  %s\n", what.c_str(), v.details.c_str(), v.MismatchText().c_str());
}

const uint32_t kSentinel = 0xDEADBEEFu;

// How a device packs the 64 threads of a group into waves.
struct Layout {
  int threads_per_wave;  // consecutive threads of a group
  int lane_count;        // what WaveGetLaneCount returns; >= threads_per_wave
  bool reversed;         // the first thread of a wave has the highest lane
};

struct Lane {
  int thread;
  uint32_t lane;
};

// The threads of the wave that thread |t| is in, by lane.
std::vector<Lane> WaveOf(int t, const Layout& layout) {
  std::vector<Lane> wave;
  const int base = t - (t % 64) % layout.threads_per_wave;
  for (int k = 0; k < layout.threads_per_wave; ++k) {
    Lane lane;
    lane.thread = layout.reversed ? base + layout.threads_per_wave - 1 - k : base + k;
    lane.lane = (uint32_t)k;
    wave.push_back(lane);
  }
  return wave;
}

uint32_t LaneOf(int t, const std::vector<Lane>& wave) {
  for (size_t k = 0; k < wave.size(); ++k)
    if (wave[k].thread == t) return wave[k].lane;
  abort();
}

// wave_ops.hlsl.
std::vector<uint32_t> EmulateWave(const std::vector<uint32_t>& src, const Layout& layout) {
  std::vector<uint32_t> dst(kThreads * kWaveRec, kSentinel);
  for (int id = 0; id < kThreads; ++id) {
    const std::vector<Lane> wave = WaveOf(id, layout);
    const uint32_t lane = LaneOf(id, wave);
    const uint32_t v = src[id];
    const bool odd = (v & 1) != 0;
    uint32_t* rec = &dst[id * kWaveRec];
    rec[0] = (uint32_t)layout.lane_count;
    rec[1] = lane;
    rec[2] = (uint32_t)wave[0].thread;
    rec[5] = src[wave[0].thread];
    rec[6] = src[wave[0].thread];
    rec[7] = wave[0].lane;
    rec[3] = rec[4] = rec[12] = rec[13] = rec[16] = rec[18] = rec[20] = rec[21] = 0;
    rec[8] = rec[9] = rec[10] = rec[11] = 0;
    rec[14] = 0xffffffffu;
    rec[15] = 0;
    rec[17] = 0xffffffffu;
    bool all_equal = true, any = false, all = true;
    rec[22] = kSentinel;  // the neighbouring lane, when the wave has none there
    for (size_t k = 0; k < wave.size(); ++k) {
      const uint32_t x = src[wave[k].thread];
      const bool x_odd = (x & 1) != 0;
      rec[3] += x;
      if (wave[k].lane < lane) rec[4] += x;
      if (x_odd) rec[8 + wave[k].lane / 32] |= 1u << (wave[k].lane % 32);
      if (x_odd) ++rec[12];
      if (x_odd && wave[k].lane < lane) ++rec[13];
      if (x < rec[14]) rec[14] = x;
      if (x > rec[15]) rec[15] = x;
      rec[16] |= x;
      rec[17] &= x;
      rec[18] ^= x;
      if (x != v) all_equal = false;
      if (x_odd) any = true;
      if (!x_odd) all = false;
      if (x_odd == odd) {  // the lanes in this thread's branch
        rec[20] += odd ? x : x * 3;
        ++rec[21];
      }
      if (wave[k].lane == (lane ^ 1)) rec[22] = x;
    }
    if (!odd) rec[21] += 1000;
    rec[19] = (all_equal ? 1u : 0u) | (any ? 2u : 0u) | (all ? 4u : 0u) | (lane == wave[0].lane ? 8u : 0u);
    rec[23] = 0x600D0000u | (uint32_t)id;
  }
  return dst;
}

// reduce.hlsl: 0 REDUCE_GROUPSHARED, 1 REDUCE_WAVE, 2 REDUCE_WAVE_ONLY.
std::vector<uint32_t> EmulateReduce(const std::vector<uint32_t>& src, int variant, const Layout& layout) {
  std::vector<uint32_t> dst(kThreads * 3, kSentinel);
  for (int id = 0; id < kThreads; ++id) {
    const int group = id / 64 * 64;
    const int gi = id % 64;
    uint32_t prefix = 0, total = 0;
    if (variant == 0) {
      for (int j = 0; j < 64; ++j) {
        total += src[group + j];
        if (j < gi) prefix += src[group + j];
      }
    } else {
      const std::vector<Lane> wave = WaveOf(id, layout);
      const uint32_t lane = LaneOf(id, wave);
      uint32_t wave_sum = 0, wave_prefix = 0;
      for (size_t k = 0; k < wave.size(); ++k) {
        wave_sum += src[wave[k].thread];
        if (wave[k].lane < lane) wave_prefix += src[wave[k].thread];
      }
      if (variant == 2) {
        prefix = wave_prefix;
        total = wave_sum;
      } else {
        // Group memory: the first thread of every thread's wave and the sum of its wave.
        const int first = wave[0].thread % 64;
        uint32_t before = 0;
        for (int j = 0; j < 64; ++j) {
          const std::vector<Lane> other = WaveOf(group + j, layout);
          if (other[0].thread % 64 != j) continue;
          uint32_t other_sum = 0;
          for (size_t k = 0; k < other.size(); ++k) other_sum += src[other[k].thread];
          total += other_sum;
          if (j < first) before += other_sum;
        }
        prefix = before + wave_prefix;
      }
    }
    dst[id * 3] = prefix;
    dst[id * 3 + 1] = total;
    dst[id * 3 + 2] = variant == 0 ? 0 : (uint32_t)layout.lane_count;
  }
  return dst;
}

// native16_math.hlsl, in 32-bit integers with the cut to 16 bits written out. |wide_float|: a float16_t
// that keeps more than 16 bits.
uint32_t U16(int64_t x) { return (uint32_t)(x & 0xffff); }
int32_t S16(int64_t x) { return (int32_t)(((x & 0xffff) ^ 0x8000) - 0x8000); }
int32_t Floor(int32_t x, int shift) { return x >= 0 ? x >> shift : -((-x + (1 << shift) - 1) >> shift); }

std::vector<uint32_t> EmulateMath(const std::vector<uint32_t>& src, bool wide_float) {
  std::vector<uint32_t> dst(kThreads * kMathRec, kSentinel);
  for (int id = 0; id < kThreads; ++id) {
    int32_t P[4], Q[4], X[3], Y[3];
    for (int k = 0; k < 4; ++k) {
      P[k] = (int32_t)((src[id * 4] >> (8 * k)) & 0xff);
      Q[k] = (int32_t)((src[id * 4 + 1] >> (8 * k)) & 0xff);
    }
    for (int k = 0; k < 3; ++k) {
      X[k] = (int32_t)((src[id * 4 + 2] >> (10 * k)) & 0x3ff);
      Y[k] = (int32_t)((src[id * 4 + 3] >> (10 * k)) & 0x3ff);
    }
    uint32_t* rec = &dst[id * kMathRec];
    for (int k = 0; k < kMathRec; ++k) rec[k] = 0;
    for (int k = 0; k < 4; ++k) {
      rec[0] |= (U16((P[k] + Q[k] + 1)) >> 1 & 0xff) << (8 * k);
      rec[2] |= (U16(P[k] * 3 + Q[k] * 13 + 8) >> 4 & 0xff) << (8 * k);
      const int32_t d = S16(P[k] - Q[k]);
      rec[3] |= (U16(Floor(d, 2)) & 0xff) << (8 * k);
      int32_t c = S16(P[k] + d * 4);
      c = c < 0 ? 0 : c > 255 ? 255 : c;
      rec[4] |= ((uint32_t)c & 0xff) << (8 * k);
    }
    rec[1] = U16(P[0] + P[1] + P[2] + P[3]) | (U16(Q[0] + Q[1] + Q[2] + Q[3]) << 16);
    rec[5] = U16((int64_t)P[0] * Q[0] * 5) | (U16((int64_t)P[1] * Q[1] * 5) << 16);
    rec[6] = (U16((int64_t)P[2] * Q[2] * 5) >> 3) | ((U16((int64_t)P[3] * Q[3] * 5) >> 3) << 16);
    const uint32_t u = U16((P[0] << 8) | Q[0]);
    rec[7] = (uint32_t)Floor(S16(u), 4);
    rec[8] = U16(Floor(S16((int64_t)u * 3), 4));
    int32_t sad = 0;
    for (int k = 0; k < 3; ++k) {
      rec[9] |= (U16(X[k] + Y[k] + 1) >> 1 & 0x3ff) << (10 * k);
      rec[10] |= (U16(X[k] * 9 + Y[k] * 7 + 8) >> 4 & 0x3ff) << (10 * k);
      const int32_t e = S16(X[k] - Y[k]);
      int32_t c = S16(X[k] + Floor(S16(e * 3), 1));
      c = c < 0 ? 0 : c > 1023 ? 1023 : c;
      rec[11] |= ((uint32_t)c & 0x3ff) << (10 * k);
      sad += e < 0 ? -e : e;
    }
    rec[12] = U16((int64_t)X[0] * Y[0]) | ((U16((int64_t)X[1] * Y[1]) >> 2) << 16);
    rec[13] = U16(sad) | (U16((int64_t)X[2] * Y[2]) << 16);
    const double f = P[0] * 0.25 + Q[0] * 0.75;
    rec[14] = (uint32_t)(f + 0.5) | ((uint32_t)(f * 4.0) << 16);
    // 2048..3071 in a 16-bit float: multiples of 2, a tie to the one whose half is even.
    const uint32_t n = 2048u + (uint32_t)X[0];
    uint32_t h = n;
    if (!wide_float && (n & 1)) h = ((n + 1) / 2) % 2 == 0 ? n + 1 : n - 1;
    rec[15] = h;
    rec[16] = 0x16B17000u | (2u << 8) | (2u << 4) | 2u;
  }
  return dst;
}

// native16_load.hlsl.
std::vector<uint32_t> EmulateLoad(const std::vector<uint32_t>& src) {
  std::vector<uint32_t> dst(kThreads * 4, kSentinel);
  const uint8_t* bytes = reinterpret_cast<const uint8_t*>(src.data());
  uint8_t* out = reinterpret_cast<uint8_t*>(dst.data());
  for (int id = 0; id < kThreads; ++id) {
    uint32_t s[4];
    for (int k = 0; k < 4; ++k) s[k] = (uint32_t)bytes[id * 8 + 2 * k] | ((uint32_t)bytes[id * 8 + 2 * k + 1] << 8);
    const uint32_t t = (uint32_t)bytes[id * 8 + 2] | ((uint32_t)bytes[id * 8 + 3] << 8);
    const uint32_t avg = U16(s[0] + s[1] + s[2] + s[3] + 2) >> 2;
    const int32_t diff = S16((int64_t)S16(s[0]) - S16(s[3]));
    const uint32_t words[2] = {avg | (t << 16), (uint32_t)diff};
    const uint32_t halves[4] = {avg, t, U16(diff), U16(0xA000 | id)};
    for (int k = 0; k < 8; ++k) out[id * 16 + k] = (uint8_t)(words[k / 4] >> (8 * (k % 4)));
    for (int k = 0; k < 8; ++k) out[id * 16 + 8 + k] = (uint8_t)(halves[k / 2] >> (8 * (k % 2)));
  }
  return dst;
}

std::string LayoutText(const Layout& layout) {
  return Fmt("%d threads in a wave of %d lanes%s", layout.threads_per_wave, layout.lane_count,
             layout.reversed ? ", reversed" : "");
}

}  // namespace

int main() {
  // basic.hlsl
  {
    const std::vector<uint32_t> in = BasicInput();
    std::vector<uint32_t> out(kThreads);
    for (int i = 0; i < kThreads; ++i) out[i] = in[i] * 3 + (uint32_t)i;
    Verdict v;
    CheckBasic(in, out, &v);
    Expect(v.mismatches == 0, "basic", v);
    for (int i = 0; i < kThreads; i += 37) {
      std::vector<uint32_t> bad = out;
      bad[i] ^= 1;
      Verdict m;
      CheckBasic(in, bad, &m);
      Expect(m.mismatches == 1, Fmt("basic, word %d changed", i), m);
    }
  }

  // wave_ops.hlsl
  const Layout layouts[] = {{4, 4, false},  {8, 8, false},   {16, 16, false}, {32, 32, false}, {64, 64, false},
                            {4, 4, true},   {32, 32, true},  {64, 64, true},  {32, 64, false}, {16, 128, true},
                            {1, 4, false}};
  for (size_t l = 0; l < sizeof(layouts) / sizeof(layouts[0]); ++l) {
    const Layout& layout = layouts[l];
    const std::vector<uint32_t> in = WaveInput();
    const std::vector<uint32_t> out = EmulateWave(in, layout);
    Verdict v;
    CheckWave(in, out, 0, 0, &v);
    Expect(v.mismatches == 0, "wave_ops, " + LayoutText(layout), v);
    Expect(v.details.find(Fmt("lane count %d;", layout.lane_count)) != std::string::npos,
           "wave_ops details name the lane count, " + LayoutText(layout), v);
    Expect(v.details.find(Fmt("%d waves", kThreads / layout.threads_per_wave)) != std::string::npos,
           "wave_ops details count the waves, " + LayoutText(layout), v);
    if (layout.threads_per_wave > 1)
      Expect((v.details.find("not in thread order") != std::string::npos) == layout.reversed,
             "wave_ops details say whether the lanes are in thread order, " + LayoutText(layout), v);
    // [WaveSize]: the lane count asked for, and another one.
    Verdict asked;
    CheckWave(in, out, layout.lane_count, layout.lane_count, &asked);
    Expect(asked.mismatches == 0, "wavesize with the lane count asked for, " + LayoutText(layout), asked);
    Verdict other;
    CheckWave(in, out, layout.lane_count * 2, layout.lane_count * 2, &other);
    Expect(other.mismatches > 0, "wavesize with another lane count than asked for, " + LayoutText(layout), other);
    Verdict range;
    CheckWave(in, out, 4, 128, &range);
    Expect(range.mismatches == 0, "wavesize range, " + LayoutText(layout), range);
    // One word of one record changed: every field, in threads of every thread group.
    for (int field = 0; field < kWaveRec; ++field) {
      for (int t = 5; t < kThreads; t += 61) {
        if (field == 22 && layout.threads_per_wave == 1) continue;  // no neighbouring lane: not judged
        std::vector<uint32_t> bad = out;
        bad[t * kWaveRec + field] ^= 1;
        Verdict m;
        CheckWave(in, bad, 0, 0, &m);
        Expect(m.mismatches > 0, Fmt("wave_ops, field %d of thread %d changed, ", field, t) + LayoutText(layout), m);
      }
    }
  }
  {
    // Nothing written.
    const std::vector<uint32_t> in = WaveInput();
    Verdict v;
    CheckWave(in, std::vector<uint32_t>(kThreads * kWaveRec, kSentinel), 0, 0, &v);
    Expect(v.mismatches == kThreads, "wave_ops, no thread wrote", v);
  }

  // reduce.hlsl
  {
    const std::vector<uint32_t> in = ReduceInput();
    const Layout one = {64, 64, false};
    for (int variant = 0; variant < 3; ++variant) {
      for (size_t l = 0; l < sizeof(layouts) / sizeof(layouts[0]); ++l) {
        const Layout& layout = variant == 1 ? layouts[l] : one;
        if (variant != 1 && l) break;
        const std::vector<uint32_t> out = EmulateReduce(in, variant, layout);
        Verdict v;
        CheckReduce(in, out, variant != 0, &v);
        const std::string name = Fmt("reduce variant %d, ", variant) + LayoutText(layout);
        if (layout.reversed) {
          // The prefix sum by lane is not the prefix sum by thread: the comparison has to say so.
          Expect(v.mismatches > 0 && v.details.find("the sums are right") != std::string::npos, name, v);
          continue;
        }
        Expect(v.mismatches == 0, name, v);
        for (int word = 0; word < 2; ++word) {
          for (int t = 3; t < kThreads; t += 59) {
            std::vector<uint32_t> bad = out;
            bad[t * 3 + word] += 1;
            Verdict m;
            CheckReduce(in, bad, variant != 0, &m);
            Expect(m.mismatches == 1, name + Fmt(", word %d of thread %d changed", word, t), m);
          }
        }
      }
    }
    // [WaveSize(64)] and nothing else, on a device whose waves are half a thread group: wrong sums.
    const Layout half = {32, 64, false};
    Verdict v;
    CheckReduce(in, EmulateReduce(in, 2, half), true, &v);
    Expect(v.mismatches > 0, "reduce_wave64 with 32 threads in a wave", v);
  }

  // native16_math.hlsl
  for (int wide = 0; wide < 2; ++wide) {
    const std::vector<uint32_t> in = MathInput();
    const std::vector<uint32_t> out = EmulateMath(in, wide != 0);
    Verdict v;
    CheckNative16Math(in, out, &v);
    Expect(v.mismatches == 0, Fmt("native16_math, wide float %d", wide), v);
    Expect(v.details.find(wide ? "wider than 16 bits" : "is a 16-bit float") != std::string::npos,
           Fmt("native16_math details, wide float %d", wide), v);
    for (int field = 0; field < kMathRec; ++field) {
      if (field == 15) continue;  // reported, not judged
      for (int bit = 0; bit < 32; bit += 13) {
        for (int t = 0; t < kThreads; t += 51) {
          std::vector<uint32_t> bad = out;
          bad[t * kMathRec + field] ^= 1u << bit;
          Verdict m;
          CheckNative16Math(in, bad, &m);
          Expect(m.mismatches == 1, Fmt("native16_math, bit %d of field %d of thread %d changed", bit, field, t), m);
        }
      }
    }
  }

  // native16_load.hlsl
  {
    const std::vector<uint32_t> in = LoadInput();
    const std::vector<uint32_t> out = EmulateLoad(in);
    Verdict v;
    CheckNative16Load(in, out, &v);
    Expect(v.mismatches == 0, "native16_load", v);
    for (int word = 0; word < 4; ++word) {
      for (int bit = 3; bit < 32; bit += 16) {
        for (int t = 1; t < kThreads; t += 63) {
          std::vector<uint32_t> bad = out;
          bad[t * 4 + word] ^= 1u << bit;
          Verdict m;
          CheckNative16Load(in, bad, &m);
          Expect(m.mismatches == 1, Fmt("native16_load, bit %d of word %d of thread %d changed", bit, word, t), m);
        }
      }
    }
  }

  if (g_failures) {
    printf("sm6test checks: %d of %d expectations FAILED\n", g_failures, g_checks);
    return 1;
  }
  printf("sm6test checks: %d expectations met: every emulated shader output passes (waves of 1 to 64 threads, lanes "
         "in order and reversed), every changed output is rejected\n",
         g_checks);
  return 0;
}
