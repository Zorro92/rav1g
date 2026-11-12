/*
 * Shader Model 6 test: the known input of every test shader and the comparison of what the shader wrote
 * with what the host computes. Plain C++, nothing of Windows: sm6test.cpp uses it on the device's output,
 * and checks_test.cpp runs it on Linux against an emulation of the shaders (testing/sm6test/run.sh).
 */

#pragma once

#include <stdarg.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>

#include <algorithm>
#include <map>
#include <string>
#include <vector>

namespace sm6check {

enum {
  kThreads = 256,  // 4 thread groups of 64
  kGroups = 4
};

inline std::string Fmt(const char* fmt, ...) {
  char text[1024];
  va_list ap;
  va_start(ap, fmt);
  vsnprintf(text, sizeof(text), fmt, ap);
  va_end(ap);
  text[sizeof(text) - 1] = 0;
  return text;
}

// The known input: the same numbers at every launch.
struct Random {
  uint32_t state;
  explicit Random(uint32_t seed) : state(seed) {}
  uint32_t Next() {
    state = state * 1664525u + 1013904223u;
    return state >> 8;
  }
};

// What a test found. |mismatches|: values that differ from what the host computed.
struct Verdict {
  int mismatches = 0;
  std::string details;      // what the device did: lane count, waves
  std::string first;        // the first few mismatches
  int first_count = 0;
  std::map<std::string, int> by_field;

  void Mismatch(const char* field, int thread, uint32_t got, uint32_t expected) {
    ++mismatches;
    ++by_field[field];
    if (first_count < 3) {
      first += Fmt("%sthread %d %s got 0x%X expected 0x%X", first_count ? "; " : "", thread, field, (unsigned)got,
                   (unsigned)expected);
      ++first_count;
    }
  }
  void Compare(const char* field, int thread, uint32_t got, uint32_t expected) {
    if (got != expected) Mismatch(field, thread, got, expected);
  }
  std::string MismatchText() const {
    if (!mismatches) return "0 mismatches";
    std::string text = Fmt("%d mismatches:", mismatches);
    for (std::map<std::string, int>::const_iterator it = by_field.begin(); it != by_field.end(); ++it)
      text += Fmt(" %s %d", it->first.c_str(), it->second);
    return text + " (" + first + ")";
  }
};

// --- basic ---

inline std::vector<uint32_t> BasicInput() {
  Random random(1);
  std::vector<uint32_t> in(kThreads);
  for (int i = 0; i < kThreads; ++i) in[i] = random.Next() & 0xffff;
  return in;
}

inline void CheckBasic(const std::vector<uint32_t>& in, const std::vector<uint32_t>& out, Verdict* v) {
  for (int i = 0; i < kThreads; ++i) v->Compare("value", i, out[i], in[i] * 3 + (uint32_t)i);
  v->details = "256 threads";
}

// --- wave intrinsics ---

enum { kWaveRec = 24 };

// 8-bit values, as samples are. Thread group 2 has one value for all its threads, an odd one, and thread
// group 3 even values only, so that the all-equal, all-true and any-true results are not the same everywhere.
inline std::vector<uint32_t> WaveInput() {
  Random random(2);
  std::vector<uint32_t> in(kThreads);
  for (int i = 0; i < kThreads; ++i) {
    const uint32_t value = random.Next() & 0xff;
    in[i] = i / 64 == 2 ? 77 : i / 64 == 3 ? (value & ~1u) : value;
  }
  return in;
}

// |lanes_min|..|lanes_max|: the lane count the shader asked for with [WaveSize], 0: it did not ask.
inline void CheckWave(const std::vector<uint32_t>& in, const std::vector<uint32_t>& out, int lanes_min, int lanes_max,
               Verdict* v) {
  // The threads that wrote their record, by wave: the wave of a thread is named by the thread number of its
  // first lane.
  std::map<uint32_t, std::vector<int> > waves;
  std::map<uint32_t, int> lane_counts;
  int unwritten = 0;
  for (int i = 0; i < kThreads; ++i) {
    const uint32_t* rec = &out[i * kWaveRec];
    if (rec[23] != (0x600D0000u | (uint32_t)i)) {
      ++unwritten;
      v->Mismatch("record", i, rec[23], 0x600D0000u | (uint32_t)i);
      continue;
    }
    ++lane_counts[rec[0]];
    if (rec[2] >= (uint32_t)kThreads) {
      v->Mismatch("WaveReadLaneFirst(thread)", i, rec[2], (uint32_t)i);
      continue;
    }
    waves[rec[2]].push_back(i);
  }

  std::map<int, int> sizes;  // wave size -> number of waves
  bool in_order = true;      // the lanes of a wave are threads in ascending order
  bool consecutive = true;   // ... and consecutive threads
  bool one_group = true;     // no wave has threads of two thread groups
  for (std::map<uint32_t, std::vector<int> >::iterator it = waves.begin(); it != waves.end(); ++it) {
    std::vector<int>& members = it->second;
    // By lane.
    std::sort(members.begin(), members.end(),
              [&](int a, int b) { return out[a * kWaveRec + 1] < out[b * kWaveRec + 1]; });
    const int n = (int)members.size();
    ++sizes[n];
    const uint32_t lane_count = out[members[0] * kWaveRec];
    const uint32_t first_lane = out[members[0] * kWaveRec + 1];
    uint32_t sum = 0, odd_count = 0, vmin = 0xffffffffu, vmax = 0, bit_or = 0, bit_and = 0xffffffffu, bit_xor = 0;
    uint32_t sum_odd = 0, sum_even3 = 0, even_count = 0;
    uint32_t ballot[4] = {0, 0, 0, 0};
    bool all_equal = true;
    std::map<uint32_t, uint32_t> value_of_lane;
    for (int k = 0; k < n; ++k) {
      const int t = members[k];
      const uint32_t lane = out[t * kWaveRec + 1];
      const uint32_t value = in[t];
      if (lane >= lane_count || lane >= 128) v->Mismatch("WaveGetLaneIndex", t, lane, lane_count);
      if (value_of_lane.count(lane)) v->Mismatch("WaveGetLaneIndex (lane twice in a wave)", t, lane, lane);
      value_of_lane[lane] = value;
      sum += value;
      vmin = (std::min)(vmin, value);
      vmax = (std::max)(vmax, value);
      bit_or |= value;
      bit_and &= value;
      bit_xor ^= value;
      if (value != in[members[0]]) all_equal = false;
      if (value & 1) {
        ++odd_count;
        sum_odd += value;
        if (lane < 128) ballot[lane / 32] |= 1u << (lane % 32);
      } else {
        ++even_count;
        sum_even3 += value * 3;
      }
      if (k && t < members[k - 1]) in_order = false;
      if (k && t != members[k - 1] + 1) consecutive = false;
      if (t / 64 != members[0] / 64) one_group = false;
    }
    if ((uint32_t)n > lane_count) v->Mismatch("threads in a wave", members[0], (uint32_t)n, lane_count);
    // The wave is named by its first lane: that is the thread of its lowest lane.
    if ((uint32_t)members[0] != it->first)
      v->Mismatch("WaveReadLaneFirst(thread)", members[0], it->first, (uint32_t)members[0]);

    uint32_t prefix = 0, prefix_odd = 0;
    for (int k = 0; k < n; ++k) {
      const int t = members[k];
      const uint32_t* rec = &out[t * kWaveRec];
      const uint32_t value = in[t];
      const bool odd = (value & 1) != 0;
      v->Compare("WaveGetLaneCount", t, rec[0], lane_count);
      v->Compare("WaveActiveSum", t, rec[3], sum);
      v->Compare("WavePrefixSum", t, rec[4], prefix);
      v->Compare("WaveReadLaneFirst", t, rec[5], in[members[0]]);
      v->Compare("WaveReadLaneAt(first)", t, rec[6], in[members[0]]);
      v->Compare("WaveReadLaneFirst(lane)", t, rec[7], first_lane);
      v->Compare("WaveActiveBallot.x", t, rec[8], ballot[0]);
      v->Compare("WaveActiveBallot.y", t, rec[9], ballot[1]);
      v->Compare("WaveActiveBallot.z", t, rec[10], ballot[2]);
      v->Compare("WaveActiveBallot.w", t, rec[11], ballot[3]);
      v->Compare("WaveActiveCountBits", t, rec[12], odd_count);
      v->Compare("WavePrefixCountBits", t, rec[13], prefix_odd);
      v->Compare("WaveActiveMin", t, rec[14], vmin);
      v->Compare("WaveActiveMax", t, rec[15], vmax);
      v->Compare("WaveActiveBitOr", t, rec[16], bit_or);
      v->Compare("WaveActiveBitAnd", t, rec[17], bit_and);
      v->Compare("WaveActiveBitXor", t, rec[18], bit_xor);
      const uint32_t flags = (all_equal ? 1u : 0u) | (odd_count ? 2u : 0u) | (odd_count == (uint32_t)n ? 4u : 0u) |
                             (k == 0 ? 8u : 0u);
      v->Compare("AllEqual/AnyTrue/AllTrue/IsFirstLane", t, rec[19], flags);
      v->Compare("WaveActiveSum in a branch", t, rec[20], odd ? sum_odd : sum_even3);
      v->Compare("WaveActiveCountBits in a branch", t, rec[21], odd ? odd_count : even_count + 1000);
      const std::map<uint32_t, uint32_t>::const_iterator neighbour = value_of_lane.find(rec[1] ^ 1);
      if (neighbour != value_of_lane.end()) v->Compare("WaveReadLaneAt(lane ^ 1)", t, rec[22], neighbour->second);
      prefix += value;
      if (odd) ++prefix_odd;
    }
  }

  // What the device did.
  std::string text;
  if (lane_counts.empty()) {
    text = "no thread wrote its record";
  } else {
    text = "lane count";
    for (std::map<uint32_t, int>::const_iterator it = lane_counts.begin(); it != lane_counts.end(); ++it)
      text += Fmt("%s %u", it == lane_counts.begin() ? "" : " and", (unsigned)it->first);
    if (lane_counts.size() > 1) {
      // One shader, one lane count: it is a property of the device and the shader, not of a wave.
      text += " (differs between threads)";
      v->Mismatch("WaveGetLaneCount differs between threads", 0, lane_counts.begin()->first,
                  lane_counts.rbegin()->first);
    }
    text += Fmt("; %d waves in 4 thread groups of 64:", (int)waves.size());
    for (std::map<int, int>::const_iterator it = sizes.begin(); it != sizes.end(); ++it)
      text += Fmt("%s %d of %d threads", it == sizes.begin() ? "" : ",", it->second, it->first);
    text += in_order ? (consecutive ? "; lanes are consecutive threads in order"
                                    : "; lanes are threads in order, not consecutive")
                     : "; lanes are not in thread order";
    text += one_group ? "; no wave across thread groups" : "; waves across thread groups";
  }
  if (unwritten) text += Fmt("; %d threads wrote no record", unwritten);
  if (lanes_min) {
    // [WaveSize]: the lane count is the one asked for.
    for (std::map<uint32_t, int>::const_iterator it = lane_counts.begin(); it != lane_counts.end(); ++it) {
      if ((int)it->first < lanes_min || (int)it->first > lanes_max)
        v->Mismatch("lane count asked for with [WaveSize]", 0, it->first, (uint32_t)lanes_min);
    }
  }
  v->details = text;
}

// --- reductions ---

inline std::vector<uint32_t> ReduceInput() {
  Random random(3);
  std::vector<uint32_t> in(kThreads);
  for (int i = 0; i < kThreads; ++i) in[i] = random.Next() & 0x3ff;
  return in;
}

inline void CheckReduce(const std::vector<uint32_t>& in, const std::vector<uint32_t>& out, bool has_lanes, Verdict* v) {
  int sum_bad = 0, prefix_bad = 0;
  std::map<uint32_t, int> lane_counts;
  for (int g = 0; g < kGroups; ++g) {
    uint32_t total = 0;
    for (int i = 0; i < 64; ++i) total += in[g * 64 + i];
    uint32_t prefix = 0;
    for (int i = 0; i < 64; ++i) {
      const int t = g * 64 + i;
      if (out[t * 3] != prefix) {
        ++prefix_bad;
        v->Mismatch("prefix sum", t, out[t * 3], prefix);
      }
      if (out[t * 3 + 1] != total) {
        ++sum_bad;
        v->Mismatch("sum", t, out[t * 3 + 1], total);
      }
      if (has_lanes) ++lane_counts[out[t * 3 + 2]];
      prefix += in[t];
    }
  }
  std::string text = "sum and prefix sum of 64 values in each of 4 thread groups";
  if (has_lanes) {
    text += "; lane count";
    for (std::map<uint32_t, int>::const_iterator it = lane_counts.begin(); it != lane_counts.end(); ++it)
      text += Fmt("%s %u", it == lane_counts.begin() ? "" : " and", (unsigned)it->first);
  }
  if (!sum_bad && prefix_bad)
    text += "; the sums are right and the prefix sums are not: the lanes of a wave are not consecutive threads of "
            "the group in order";
  v->details = text;
}

// --- native 16-bit types ---

enum { kMathRec = 17 };

inline int16_t Asr16(int16_t value, int shift) {  // arithmetic shift right, whatever the compiler's >> does
  return (int16_t)(value >= 0 ? value >> shift : ~((~value) >> shift));
}

inline int16_t Clamp16(int16_t value, int16_t lo, int16_t hi) { return value < lo ? lo : value > hi ? hi : value; }

inline std::vector<uint32_t> MathInput() {
  Random random(4);
  std::vector<uint32_t> in(kThreads * 4);
  for (int i = 0; i < kThreads; ++i) {
    in[i * 4] = (random.Next() << 16) ^ random.Next();
    in[i * 4 + 1] = (random.Next() << 16) ^ random.Next();
    in[i * 4 + 2] = ((random.Next() << 16) ^ random.Next()) & 0x3fffffffu;
    in[i * 4 + 3] = ((random.Next() << 16) ^ random.Next()) & 0x3fffffffu;
  }
  // The ends of the ranges.
  static const uint32_t kEdges[4][4] = {{0, 0, 0, 0},
                                        {0xffffffffu, 0xffffffffu, 0x3fffffffu, 0x3fffffffu},
                                        {0, 0xffffffffu, 0, 0x3fffffffu},
                                        {0xffffffffu, 0, 0x3fffffffu, 0}};
  memcpy(in.data(), kEdges, sizeof(kEdges));
  return in;
}

inline void CheckNative16Math(const std::vector<uint32_t>& in, const std::vector<uint32_t>& out, Verdict* v) {
  int half = 0, wide = 0, other = 0;
  for (int i = 0; i < kThreads; ++i) {
    uint16_t P[4], Q[4], X[3], Y[3], t4[4], t3[3];
    for (int k = 0; k < 4; ++k) {
      P[k] = (uint16_t)((in[i * 4] >> (8 * k)) & 0xff);
      Q[k] = (uint16_t)((in[i * 4 + 1] >> (8 * k)) & 0xff);
    }
    for (int k = 0; k < 3; ++k) {
      X[k] = (uint16_t)((in[i * 4 + 2] >> (10 * k)) & 0x3ff);
      Y[k] = (uint16_t)((in[i * 4 + 3] >> (10 * k)) & 0x3ff);
    }
    auto pack8 = [](const uint16_t* x) {
      return (uint32_t)(x[0] & 0xff) | ((uint32_t)(x[1] & 0xff) << 8) | ((uint32_t)(x[2] & 0xff) << 16) |
             ((uint32_t)(x[3] & 0xff) << 24);
    };
    auto pack10 = [](const uint16_t* x) {
      return (uint32_t)(x[0] & 0x3ff) | ((uint32_t)(x[1] & 0x3ff) << 10) | ((uint32_t)(x[2] & 0x3ff) << 20);
    };
    uint32_t e[kMathRec] = {};
    // Every step is cut to 16 bits, as the shader's uint16_t and int16_t are.
    for (int k = 0; k < 4; ++k) t4[k] = (uint16_t)((uint16_t)(P[k] + Q[k] + 1) >> 1);
    e[0] = pack8(t4);
    e[1] = (uint32_t)(uint16_t)(P[0] + P[1] + P[2] + P[3]) | ((uint32_t)(uint16_t)(Q[0] + Q[1] + Q[2] + Q[3]) << 16);
    for (int k = 0; k < 4; ++k) t4[k] = (uint16_t)((uint16_t)(P[k] * 3 + Q[k] * 13 + 8) >> 4);
    e[2] = pack8(t4);
    int16_t D[4];
    for (int k = 0; k < 4; ++k) D[k] = (int16_t)(uint16_t)(P[k] - Q[k]);
    for (int k = 0; k < 4; ++k) t4[k] = (uint16_t)Asr16(D[k], 2);
    e[3] = pack8(t4);
    for (int k = 0; k < 4; ++k)
      t4[k] = (uint16_t)Clamp16((int16_t)(uint16_t)(P[k] + (uint16_t)(D[k] * 4)), 0, 255);
    e[4] = pack8(t4);
    uint16_t W[4];
    for (int k = 0; k < 4; ++k) W[k] = (uint16_t)((uint32_t)P[k] * Q[k] * 5);
    e[5] = (uint32_t)W[0] | ((uint32_t)W[1] << 16);
    e[6] = (uint32_t)(uint16_t)(W[2] >> 3) | ((uint32_t)(uint16_t)(W[3] >> 3) << 16);
    const uint16_t u = (uint16_t)((P[0] << 8) | Q[0]);
    e[7] = (uint32_t)(int32_t)Asr16((int16_t)u, 4);
    e[8] = (uint32_t)(uint16_t)Asr16((int16_t)(uint16_t)(u * 3), 4);
    for (int k = 0; k < 3; ++k) t3[k] = (uint16_t)((uint16_t)(X[k] + Y[k] + 1) >> 1);
    e[9] = pack10(t3);
    for (int k = 0; k < 3; ++k) t3[k] = (uint16_t)((uint16_t)(X[k] * 9 + Y[k] * 7 + 8) >> 4);
    e[10] = pack10(t3);
    int16_t E[3];
    for (int k = 0; k < 3; ++k) E[k] = (int16_t)(uint16_t)(X[k] - Y[k]);
    for (int k = 0; k < 3; ++k)
      t3[k] = (uint16_t)Clamp16((int16_t)(uint16_t)(X[k] + (uint16_t)Asr16((int16_t)(E[k] * 3), 1)), 0, 1023);
    e[11] = pack10(t3);
    uint16_t M[3];
    for (int k = 0; k < 3; ++k) M[k] = (uint16_t)((uint32_t)X[k] * Y[k]);
    e[12] = (uint32_t)M[0] | ((uint32_t)(uint16_t)(M[1] >> 2) << 16);
    uint16_t sad = 0;
    for (int k = 0; k < 3; ++k) sad = (uint16_t)(sad + (uint16_t)(E[k] < 0 ? -E[k] : E[k]));
    e[13] = (uint32_t)sad | ((uint32_t)M[2] << 16);
    // f = P.x / 4 + Q.x * 3 / 4, exact: in quarters, P.x + 3 * Q.x.
    const uint32_t quarters = (uint32_t)P[0] + 3u * Q[0];
    e[14] = ((quarters + 2) >> 2) | (quarters << 16);
    e[16] = 0x16B17000u | (2u << 8) | (2u << 4) | 2u;
    static const char* const kNames[kMathRec] = {"8-bit average",
                                                 "8-bit sums",
                                                 "8-bit weighted sum",
                                                 "8-bit difference >> 2",
                                                 "8-bit clamp",
                                                 "uint16_t product (wraps)",
                                                 "uint16_t product (wraps) >> 3",
                                                 "int16_t >> 4, sign-extended",
                                                 "uint16_t product (wraps) as int16_t >> 4",
                                                 "10-bit average",
                                                 "10-bit weighted sum",
                                                 "10-bit clamp",
                                                 "10-bit uint16_t product (wraps), >> 2",
                                                 "10-bit sum of absolute differences",
                                                 "float16_t weighted sum",
                                                 "",
                                                 "sizeof 16-bit types"};
    for (int k = 0; k < kMathRec; ++k)
      if (k != 15) v->Compare(kNames[k], i, out[i * kMathRec + k], e[k]);
    // 2048 + X.x through a float16_t: a 16-bit float keeps even numbers only (an odd one goes to the
    // neighbour whose half is even), a wider float keeps the number.
    const uint32_t n = 2048u + X[0];
    const uint32_t as_half = (n & 1) ? (((n - 1) / 2) % 2 == 0 ? n - 1 : n + 1) : n;
    const uint32_t got = out[i * kMathRec + 15];
    if (n & 1) {
      if (got == as_half)
        ++half;
      else if (got == n)
        ++wide;
      else
        ++other;
    } else if (got != n) {
      ++other;
    }
  }
  v->details = "uint16_t, int16_t and float16_t arithmetic on 8-bit and 10-bit samples, 256 threads; float16_t ";
  if (other)
    v->details += Fmt("conversions of odd numbers above 2048: %d as a 16-bit float, %d as a wider float, %d neither",
                      half, wide, other);
  else if (wide && !half)
    v->details += "is wider than 16 bits (odd numbers above 2048 are kept)";
  else if (half && !wide)
    v->details += "is a 16-bit float (odd numbers above 2048 are rounded to even)";
  else
    v->details += Fmt("conversions of odd numbers above 2048: %d as a 16-bit float, %d as a wider float", half, wide);
}

// 16-bit samples, four a thread. Threads 0..191: 10-bit samples. Threads 192..255: the second and third
// sample use all 16 bits, so that the sum of the four wraps; the first and the fourth stay below 2^14, so
// that their signed difference does not overflow (that would be undefined in the shader).
inline std::vector<uint32_t> LoadInput() {
  Random random(5);
  std::vector<uint32_t> in(kThreads * 2);
  uint16_t* samples = reinterpret_cast<uint16_t*>(in.data());
  for (int i = 0; i < kThreads; ++i) {
    for (int k = 0; k < 4; ++k) {
      const uint32_t value = random.Next();
      samples[i * 4 + k] = (uint16_t)(i < 192 ? (value & 0x3ff) : (k == 1 || k == 2) ? (value & 0xffff) : (value & 0x3fff));
    }
  }
  return in;
}

inline void CheckNative16Load(const std::vector<uint32_t>& in, const std::vector<uint32_t>& out, Verdict* v) {
  const uint16_t* samples = reinterpret_cast<const uint16_t*>(in.data());
  const uint16_t* stored = reinterpret_cast<const uint16_t*>(out.data());
  for (int i = 0; i < kThreads; ++i) {
    const uint16_t* s = samples + i * 4;
    const uint16_t avg = (uint16_t)((uint16_t)(s[0] + s[1] + s[2] + s[3] + 2) >> 2);
    const int16_t diff = (int16_t)(uint16_t)(s[0] - s[3]);
    v->Compare("16-bit vector load (32-bit store)", i, out[i * 4] & 0xffff, avg);
    v->Compare("16-bit load at an address 2 mod 4 (32-bit store)", i, out[i * 4] >> 16, s[1]);
    v->Compare("int16_t difference, sign-extended (32-bit store)", i, out[i * 4 + 1], (uint32_t)(int32_t)diff);
    v->Compare("16-bit store", i, stored[i * 8 + 4], avg);
    v->Compare("16-bit store at an address 2 mod 4", i, stored[i * 8 + 5], s[1]);
    v->Compare("int16_t store", i, stored[i * 8 + 6], (uint16_t)diff);
    v->Compare("16-bit store at an address 2 mod 4", i, stored[i * 8 + 7], (uint16_t)(0xA000 | i));
  }
  v->details = "16-bit loads and stores of 16-bit samples in a byte address buffer, 256 threads";
}

}  // namespace sm6check
