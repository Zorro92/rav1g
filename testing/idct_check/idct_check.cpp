// SPDX-License-Identifier: BSD-2-Clause

#define GROUP_SCHED_IMPLEMENTATION
#include "hlsl.h"

#include <algorithm>
#include <array>
#include <cstdio>
#include <random>
#include <vector>

long g_out_of_bounds = 0;

static void scramble_bytes(void *buffer, size_t size, uint64_t *state) {
  auto *bytes = static_cast<uint8_t *>(buffer);
  for (size_t i = 0; i < size; ++i) {
    *state = *state * 6364136223846793005ULL + 1;
    bytes[i] = *state >> 56;
  }
}

namespace legacy {
#include LEGACY
#include LEGACY_ENTRY
constexpr uint kLanesPerBlock = N;
constexpr uint kWords = ScanSize;
}
#undef N
#undef ScanSize
#undef IdctOutputShiftH
#undef IdctOutputShiftV
#undef IDCT_IDENTITY_LOOP

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
static_assert(kSamples == 4 * legacy::kWords);

struct Case {
  uint start, count, depth;
  std::array<uint4, 3> planes;
  std::vector<uint> records;
  std::vector<uint> coefs, expected_coefs;
  std::vector<uint> legacy_residuals, candidate_residuals;

  Case(uint count, uint depth, uint distribution, std::mt19937 &rng)
      : start(1 + rng() % 8), count(count), depth(depth),
        records(4 * (start + count + 1), 0xdadadada), coefs(kGuardWords, 0x51515151),
        expected_coefs(coefs), legacy_residuals((3 * kPlaneBytes + 512) / 4) {
    for (auto &word : legacy_residuals) word = rng();
    candidate_residuals = legacy_residuals;
    for (uint plane = 0; plane < planes.size(); ++plane) {
      planes[plane] = uint4(384 + 64 * plane, 256 + kPlaneBytes * plane, 0, 0);
    }

    // Allocate in a different order than the sorted records, with no slack between blocks.
    std::vector<uint> allocation_order(count);
    for (uint i = 0; i < count; ++i) allocation_order[i] = i;
    std::shuffle(allocation_order.begin(), allocation_order.end(), rng);
    for (uint i : allocation_order) {
      const Type &type = kTypes[i % kTypeCount];
      const uint words = 1 + (i / kTypeCount) % legacy::kWords;
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

static void run_legacy(Case &test, std::vector<uint> &coefs, uint64_t &random_order) {
  using namespace legacy;
  buf_blocks = read_buffer(test.records);
  buf_input = write_buffer(coefs);
  buf_dst = write_buffer(test.legacy_residuals);
  cb_bitdepth = test.depth;
  cb_index_offset = test.start;
  cb_wicount = test.count * kLanesPerBlock;
  for (uint p = 0; p < 3; ++p) cb_planes.data[p] = test.planes[p];
  for (uint order = 0; order < 3; ++order) {
    for (uint i = 0; i < kWords; ++i) {
      const int *s = kScans[order] + 4 * i;
      cb_scans.data[order * kWords + i] = int4(s[0], s[1], s[2], s[3]);
    }
  }

  gs_state.shader = "legacy IDCT";
  gs_state.track_unwritten = true;
  const uint groups = (cb_wicount + kThreadsX - 1) / kThreadsX;
  const auto entry = [](int lane, void *ctx) {
    shader_entry(uint3(*static_cast<const uint *>(ctx), 0, 0), uint3(lane, 0, 0));
  };
  for (uint g = 0; g < groups; ++g) {
    shader_scramble(&random_order);
    gs_run_group(kThreadsX, entry, &g, &random_order);
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
  uint64_t random_order = 0x78e235f33;
  uint blocks = 0;
  for (uint depth : {8u, 10u}) {
    for (uint distribution = 0; distribution < 4; ++distribution) {
      // Include both partial and complete groups, and every type with every coefficient word count.
      for (uint count : {1u, 7u, 64u, 65u, kTypeCount * legacy::kWords + 1}) {
        Case test(count, depth, distribution, rng);
        auto legacy_coefs = test.coefs;
        auto candidate_coefs = test.coefs;
        run_legacy(test, legacy_coefs, random_order);
        run_candidate(test, candidate_coefs);
        if (gs_state.races || gs_state.sync_errors || gs_state.out_of_bounds || g_out_of_bounds) {
          gs_print_races("  ");
          std::fprintf(stderr, "idct_check: %ld races, %ld sync errors, %ld out-of-bounds accesses\n",
                       gs_state.races, gs_state.sync_errors, gs_state.out_of_bounds + g_out_of_bounds);
          return 2;
        }
        compare(test.expected_coefs, legacy_coefs, test, "coefficient (legacy)");
        compare(test.expected_coefs, candidate_coefs, test, "coefficient");
        compare(test.legacy_residuals, test.candidate_residuals, test, "residual");
        blocks += count;
      }
    }
  }
  std::printf("idct_check: %u blocks of %ux%u matched, all 16 types at 8 and 10 bits\n", blocks, TX_WIDTH, TX_HEIGHT);
}
