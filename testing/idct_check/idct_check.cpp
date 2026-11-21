// SPDX-License-Identifier: BSD-2-Clause

#define GROUP_SCHED_IMPLEMENTATION
#include "hlsl.h"

#include <algorithm>
#include <array>
#include <cmath>
#include <cstdio>
#include <random>
#include <vector>

long g_out_of_bounds = 0;

namespace candidate {
#include CANDIDATE
#include CANDIDATE_ENTRY
}

#include SCANS

// The 16 AV1 transform types, in bitstream order. A flipped ADST is type 1 with a flip bit.
enum Basis : uint { Dct = 0, Adst = 1, Identity = 2 };
struct Type {
  Basis horizontal, vertical;
  uint scan;
  bool flip_x = false, flip_y = false;
};
constexpr Type kTypes[] = {
    {Dct, Dct, 0}, {Dct, Adst, 0}, {Adst, Dct, 0}, {Adst, Adst, 0},
    {Dct, Adst, 0, false, true}, {Adst, Dct, 0, true}, {Adst, Adst, 0, true, true},
    {Adst, Adst, 0, true}, {Adst, Adst, 0, false, true}, {Identity, Identity, 0},
    {Identity, Dct, 1}, {Dct, Identity, 2}, {Identity, Adst, 1}, {Adst, Identity, 2},
    {Identity, Adst, 1, false, true}, {Adst, Identity, 2, true},
};

constexpr uint kTypeCount = std::size(kTypes);
constexpr uint kSamples = TX_WIDTH * TX_HEIGHT;
constexpr uint kPlaneBytes = 32768;
constexpr uint kGuardWords = 16;
constexpr uint kWords = kSamples / 4;

struct Case {
  uint start, count, depth;
  std::array<uint4, 3> planes;
  std::vector<uint> records;
  std::vector<uint> coefs, expected_coefs;
  std::vector<uint> expected_residuals, candidate_residuals;

  Case(uint count, uint depth, uint distribution, std::mt19937 &rng)
      : start(1 + rng() % 8), count(count), depth(depth),
        records(4 * (start + count + 1), 0xdadadada), coefs(kGuardWords, 0x51515151),
        expected_coefs(coefs), expected_residuals((3 * kPlaneBytes + 512) / 4) {
    for (auto &word : expected_residuals) word = rng();
    candidate_residuals = expected_residuals;
    for (uint plane = 0; plane < planes.size(); ++plane) {
      planes[plane] = uint4(384 + 64 * plane, 256 + kPlaneBytes * plane, 0, 0);
    }

    // Allocate in a different order than the sorted records, with no slack between blocks.
    std::vector<uint> allocation_order(count);
    for (uint i = 0; i < count; ++i) allocation_order[i] = i;
    std::shuffle(allocation_order.begin(), allocation_order.end(), rng);
    for (uint i : allocation_order) {
      const Type &type = kTypes[i % kTypeCount];
      const uint words = 1 + (i / kTypeCount) % kWords;
      const uint eob = 4 * words - (rng() % 4);
      const uint plane = (i + distribution) % 3;
      const uint x4 = 1 + (i / 3 % 16) * (TX_WIDTH / 4);
      const uint y4 = 1 + (i / 3 / 16) * (TX_HEIGHT / 4);
      uint *block = &records[4 * (start + i)];
      block[0] = words | (type.scan << ScanShift) | (uint(type.horizontal) << IdctType_H_Shift) |
                 (uint(type.vertical) << IdctType_V_Shift) | (plane << 21) |
                 (type.flip_x ? IdctType_H_Flip : 0) | (type.flip_y ? IdctType_V_Flip : 0);
      block[1] = coefs.size();
      block[2] = x4 | (y4 << 16);
      block[3] = rng();
      const int clamp_limit = 1 << (depth + 7);
      for (uint k = 0; k < 4 * words; ++k) {
        int value = 0;
        if (k < eob) {
          switch (distribution % 4) {
            case 0: value = int(rng() % 1025) - 512; break;
            case 1: value = rng() % 3 == 0 ? int(rng() % 65537) - 32768 : 0; break;
            case 2: value = (rng() % 2 ? -1 : 1) * (clamp_limit + int(rng() % 17) - 8); break;
            case 3: value = int(rng() % (8 * clamp_limit)) - 4 * clamp_limit; break;
          }
        }
        coefs.push_back(uint(value));
        expected_coefs.push_back(0);
      }
    }
    coefs.insert(coefs.end(), kGuardWords, 0x9f9f9f9f);
    expected_coefs.insert(expected_coefs.end(), kGuardWords, 0x9f9f9f9f);
  }
};

static ByteAddressBuffer read_buffer(const std::vector<uint> &v) {
  return {reinterpret_cast<const uint8_t *>(v.data()), v.size() * sizeof(uint)};
}

static RWByteAddressBuffer write_buffer(std::vector<uint> &v) {
  return {reinterpret_cast<uint8_t *>(v.data()), v.size() * sizeof(uint)};
}

static int rounded(int64_t value, int shift) {
  return shift == 0 ? int(value) : int((value + (int64_t(1) << (shift - 1))) >> shift);
}

static int clip_signed(int value, int bits) {
  const int limit = 1 << (bits - 1);
  return std::clamp(value, -limit, limit - 1);
}

// Scalar butterflies and permutations of AV1 7.13.2. Use the cosine definition
// directly and a coefficient matrix for ADST4, independent of the HLSL tables and stages.
static void reference_1d(int *line, int size, Basis basis, int bits, int shift) {
  const auto cos128 = [](int angle) {
    return int(std::lround(4096 * std::cos(angle * (std::acos(-1.0) / 128))));
  };
  const auto rotate = [&](int i, int j, int angle, bool swap) {
    const int c = cos128(angle), s = cos128(angle - 64);
    const int a = rounded(int64_t(line[i]) * c - int64_t(line[j]) * s, 12);
    const int b = rounded(int64_t(line[i]) * s + int64_t(line[j]) * c, 12);
    line[i] = swap ? b : a;
    line[j] = swap ? a : b;
  };
  const auto hadamard = [&](int i, int j) {
    const int a = line[i], b = line[j];
    line[i] = clip_signed(a + b, bits);
    line[j] = clip_signed(a - b, bits);
  };

  if (basis == Dct) {
    const int order4[] = {0, 2, 1, 3};
    const int order8[] = {0, 4, 2, 6, 1, 5, 3, 7};
    int input[8];
    std::copy_n(line, size, input);
    for (int i = 0; i < size; ++i) line[i] = input[size == 4 ? order4[i] : order8[i]];
    if (size == 8) {
      rotate(4, 7, 56, false);
      rotate(5, 6, 24, false);
      hadamard(4, 5);
      hadamard(7, 6);
    }
    rotate(0, 1, 32, true);
    rotate(2, 3, 48, false);
    hadamard(0, 3);
    hadamard(1, 2);
    if (size == 8) {
      rotate(6, 5, 32, true);
      for (int i = 0; i < 4; ++i) hadamard(i, 7 - i);
    }
  } else if (basis == Adst && size == 8) {
    int input[8];
    std::copy_n(line, size, input);
    for (int i = 0; i < 8; ++i) line[i] = input[i % 2 ? i - 1 : 7 - i];
    for (int i = 0; i < 4; ++i) rotate(2 * i, 2 * i + 1, 60 - 16 * i, true);
    for (int i = 0; i < 4; ++i) hadamard(i, 4 + i);
    rotate(4, 5, 48, true);
    rotate(7, 6, 16, true);
    for (int base : {0, 4}) {
      hadamard(base, base + 2);
      hadamard(base + 1, base + 3);
      rotate(base + 2, base + 3, 32, true);
    }
    std::copy_n(line, size, input);
    for (int i = 0; i < 8; ++i) {
      const int order[] = {0, 4, 6, 2, 3, 7, 5, 1};
      line[i] = (i % 2 ? -1 : 1) * input[order[i]];
    }
  } else if (basis == Adst) {
    constexpr int a = 1321, b = 2482, c = 3344, d = 3803;
    constexpr int weights[4][4] = {{a, c, d, b}, {b, c, -a, -d}, {c, 0, -c, c}, {a + b, -c, d - a, b - d}};
    int result[4];
    for (int row = 0; row < 4; ++row) {
      int64_t value = 0;
      for (int col = 0; col < 4; ++col) value += int64_t(weights[row][col]) * line[col];
      result[row] = rounded(value, 12);
    }
    std::copy_n(result, 4, line);
  } else {
    for (int i = 0; i < size; ++i) line[i] = size == 4 ? rounded(int64_t(line[i]) * 5793, 12) : 2 * line[i];
  }
  for (int i = 0; i < size; ++i) {
    line[i] = rounded(line[i], shift);
    // The shader also saturates the identity result for out-of-range coefficients.
    if (basis == Identity) line[i] = clip_signed(line[i], bits);
  }
}

static void run_reference(Case &test) {
  constexpr int width = TX_WIDTH, height = TX_HEIGHT;
  const int row_bits = test.depth + 8;
  auto *residuals = reinterpret_cast<uint8_t *>(test.expected_residuals.data());
  for (uint i = 0; i < test.count; ++i) {
    const uint *block = &test.records[4 * (test.start + i)];
    const Type &type = kTypes[i % kTypeCount];
    int pixels[height][width] = {};
    const uint count = 4 * (block[0] & 0x7ff);
    for (uint k = 0; k < count; ++k) {
      int coefficient = clip_signed(int(test.coefs[block[1] + k]), row_bits);
      if (width != height) coefficient = rounded(int64_t(coefficient) * 2896, 12);
      const int at = kScans[type.scan][k];
      pixels[at / width][at % width] = coefficient;
    }
    for (auto &row : pixels) reference_1d(row, width, type.horizontal, row_bits, width == 8 && height == 8 ? 1 : 0);
    const auto &plane = test.planes[(block[0] >> 21) & 3];
    const uint base = plane.y + (block[2] & 0xffff) * 8 + (block[2] >> 16) * 4 * plane.x;
    for (int x = 0; x < width; ++x) {
      int column[height];
      for (int y = 0; y < height; ++y) column[y] = clip_signed(pixels[y][x], 16);
      reference_1d(column, height, type.vertical, 16, 4);
      const int dx = type.flip_x ? width - 1 - x : x;
      for (int y = 0; y < height; ++y) {
        const int dy = type.flip_y ? height - 1 - y : y;
        const uint16_t value = uint16_t(column[y]);
        memcpy(residuals + base + dy * plane.x + 2 * dx, &value, sizeof(value));
      }
    }
  }
}

static void run_candidate(Case &test, std::vector<uint> &coefs) {
  using namespace candidate;
  buf_blocks = read_buffer(test.records);
  buf_input = write_buffer(coefs);
  buf_dst = write_buffer(test.candidate_residuals);
  cb_bitdepth = test.depth;
  cb_index_offset = test.start;
  cb_wicount = test.count;
  for (uint p = 0; p < 3; ++p) cb_planes.data[p] = test.planes[p];

  static_assert(kGroupMemoryArrays == 0);
  const uint groups = (cb_wicount + kThreadsX - 1) / kThreadsX;
  for (uint g = 0; g < groups; ++g) {
    for (int lane = kThreadsX - 1; lane >= 0; --lane) shader_entry(uint3(g, 0, 0), uint3(lane, 0, 0));
  }
}

static void compare(const std::vector<uint> &expected, const std::vector<uint> &actual,
                    const Case &test, const char *buffer) {
  const auto [a, b] = std::mismatch(expected.begin(), expected.end(), actual.begin());
  if (a == expected.end()) return;
  std::fprintf(stderr, "MISMATCH %s word %zu for %ux%u %u-bit, %u blocks: %08x vs %08x\n",
               buffer, size_t(a - expected.begin()), TX_WIDTH, TX_HEIGHT, test.depth, test.count, *a, *b);
  std::exit(1);
}

int main() {
  std::mt19937 rng(0xdcc4469);
  uint blocks = 0;
  for (uint depth : {8u, 10u}) {
    for (uint distribution = 0; distribution < 4; ++distribution) {
      // Include both partial and complete groups, and every type with every coefficient word count.
      for (uint count : {1u, 7u, 64u, 65u, kTypeCount * kWords + 1}) {
        Case test(count, depth, distribution, rng);
        auto candidate_coefs = test.coefs;
        run_reference(test);
        run_candidate(test, candidate_coefs);
        if (gs_state.races || gs_state.sync_errors || gs_state.out_of_bounds || g_out_of_bounds) {
          gs_print_races("  ");
          std::fprintf(stderr, "idct_check: %ld races, %ld sync errors, %ld out-of-bounds accesses\n",
                       gs_state.races, gs_state.sync_errors, gs_state.out_of_bounds + g_out_of_bounds);
          return 2;
        }
        compare(test.expected_coefs, candidate_coefs, test, "coefficient");
        compare(test.expected_residuals, test.candidate_residuals, test, "residual");
        blocks += count;
      }
    }
  }
  std::printf("idct_check: %u blocks of %ux%u matched the scalar reference, all 16 types at 8 and 10 bits\n", blocks, TX_WIDTH, TX_HEIGHT);
}
