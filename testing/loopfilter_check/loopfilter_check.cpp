// SPDX-License-Identifier: BSD-2-Clause
#define GROUP_SCHED_IMPLEMENTATION
#include "shader_adapter.h"

#include <array>
#include <random>
#include <vector>

namespace {
constexpr int kPad = 16;
constexpr uint kGuard = 0xecececec;
constexpr uint kListBase = 5;
constexpr int kStrip = 16;
constexpr int kGroup = 64;
std::mt19937 rng(0x810db10c);

struct Image {
  int width, height, depth, bytes, stride;
  std::vector<uint> words;

  Image(int width, int height, int depth)
      : width(width), height(height), depth(depth), bytes(depth == 8 ? 1 : 2),
        stride(((width + 2 * kPad) * bytes + 63) & ~63),
        words((height + 2 * kPad) * stride / 4, kGuard) {}

  void set(int x, int y, uint value) {
    const int address = (y + kPad) * stride + (x + kPad) * bytes;
    const uint shift = (address & 3) * 8;
    const uint mask = (1u << depth) - 1;
    uint &word = words[address / 4];
    word = (word & ~(((1u << (8 * bytes)) - 1) << shift)) | ((value & mask) << shift);
  }

  RWByteAddressBuffer buffer() { return {{words.data(), words.size()}, words.data()}; }
};

struct EdgeLists {
  int block_cols, block_rows;
  std::vector<uint> words;

  EdgeLists(int mi_cols, int mi_rows, bool horizontal)
      : block_cols(horizontal ? mi_cols : (mi_cols + 15) / 16),
        block_rows(horizontal ? (mi_rows + 15) / 16 : mi_rows),
        words((kListBase + block_cols * block_rows + 2) * 8, 0) {}

  uint blocks() const { return block_cols * block_rows; }

  uint block_id(bool horizontal, uint strip, uint across) const {
    return horizontal ? strip * block_cols + across : across * block_cols + strip;
  }

  void set(uint block, uint pos, uint step, uint type, uint level) {
    const uint edge = (step << 8) | (type << 6) | level;
    words[(kListBase + block) * 8 + pos / 2] |= edge << (16 * (pos & 1));
  }

  ByteAddressBuffer buffer() const { return {words.data(), words.size()}; }
};

void fail(const char *label, const Image &a, const Image &b) {
  for (size_t i = 0; i < a.words.size(); ++i) {
    if (a.words[i] == b.words[i]) continue;
    const int y = (4 * i / a.stride) - kPad;
    const int x = (4 * i % a.stride) / a.bytes - kPad;
    std::fprintf(stderr, "FAIL: %s (%d-bit) at (%d,%d): expected %08x, got %08x\n",
                 label, a.depth, x, y, a.words[i], b.words[i]);
    std::exit(1);
  }
}

FilterDispatch make_dispatch(Image &image, const EdgeLists &edges, int sharpness, uint plane) {
  FilterDispatch d{};
  d.frame = image.buffer();
  d.edges = edges.buffer();
  d.plane = plane;
  d.planes[plane] = {image.stride, kPad * image.stride + kPad * image.bytes, 0, 0};
  d.list_base = kListBase;
  d.block_cols = edges.block_cols;
  for (int level = 0; level < 64; ++level) {
    int inside = level >> ((sharpness > 0) + (sharpness > 4));
    if (sharpness > 0) inside = std::min(inside, 9 - sharpness);
    inside = std::max(inside, 1);
    d.limits[level] = {inside, 2 * (level + 2) + inside, level >> 4, 0};
  }
  return d;
}

template <class Kernel, class Dispatch>
void dispatch(const Kernel &kernel, const Dispatch &d, bool shuffle) {
  kernel.bind(d);
  gs_state.shader = kernel.name;
  std::vector<uint> groups;
  for (uint base = 0; base < d.work_items; base += kGroup) groups.push_back(base);
  if (shuffle) std::shuffle(groups.begin(), groups.end(), rng);
  uint64_t order = rng();
  for (uint base : groups) gs_run_group(kGroup, kernel.entry, &base, &order);
  if (gs_state.races || gs_state.out_of_bounds || gs_state.runaway_groups ||
      gs_state.sync_errors || gs_state.plain_barriers) {
    gs_print_races("  ");
    std::fprintf(stderr, "FAIL: group scheduling or shared memory in %s\n", kernel.name);
    std::exit(1);
  }
}

Image compare_vertical(const Image &initial, const EdgeLists &edges, int sharpness, uint plane,
                       bool expect_change = true) {
  const FilterKernel &old = initial.depth == 8 ? loopfilter_vert_legacy : loopfilter_vert_hbd_legacy;
  const FilterKernel &now = initial.depth == 8 ? loopfilter_vert : loopfilter_vert_hbd;
  Image reference = initial;
  Image candidate = initial;
  auto old_dispatch = make_dispatch(reference, edges, sharpness, plane);
  auto new_dispatch = make_dispatch(candidate, edges, sharpness, plane);
  for (uint parity = 0; parity < 2; ++parity) {
    old_dispatch.work_items = new_dispatch.work_items = (edges.blocks() + 1 - parity) / 2 * 4;
    old_dispatch.block_offset = new_dispatch.block_offset = parity;
    dispatch(old, old_dispatch, false);
    std::vector<WordAccess> ownership(candidate.words.size());
    new_dispatch.frame.access = ownership.data();
    dispatch(now, new_dispatch, true);
    fail(parity == 0 ? "vertical even" : "vertical odd", reference, candidate);
  }
  if (expect_change && reference.words == initial.words) {
    std::fprintf(stderr, "FAIL: vertical fixture made no changes\n");
    std::exit(1);
  }
  return candidate;
}

// The modes of -lfhsplit: whole plane, even rows then odd rows, and rows in order.
void run_horizontal(const FilterKernel &kernel, Image &image, const EdgeLists &edges, int sharpness,
                    uint plane, int mode, uint threads_per_strip, bool candidate) {
  auto d = make_dispatch(image, edges, sharpness, plane);
  const int passes = mode == 0 ? 1 : mode == 1 ? 2 : edges.block_rows;
  const int step = mode == 1 ? 2 : edges.block_rows;
  std::vector<WordAccess> ownership(candidate ? image.words.size() : 0);
  d.frame.access = candidate ? ownership.data() : nullptr;

  for (int pass = 0; pass < passes; ++pass) {
    for (int row = pass; row < edges.block_rows; row += step) {
      d.work_items = (mode == 0 ? edges.blocks() : edges.block_cols) * threads_per_strip;
      d.block_offset = row * edges.block_cols;
      std::fill(ownership.begin(), ownership.end(), WordAccess{});
      dispatch(kernel, d, candidate);
    }
  }
}

void compare_horizontal(const Image &initial, const EdgeLists &edges, int sharpness, uint plane, int mode,
                        bool expect_change = true) {
  const FilterKernel &old = initial.depth == 8 ? loopfilter_hor_legacy : loopfilter_hor_hbd_legacy;
  const FilterKernel &now = initial.depth == 8 ? loopfilter_hor : loopfilter_hor_hbd;
  Image reference = initial;
  Image candidate = initial;
  run_horizontal(old, reference, edges, sharpness, plane, mode, 4, false);
  run_horizontal(now, candidate, edges, sharpness, plane, mode, initial.bytes, true);
  fail("horizontal", reference, candidate);
  if (expect_change && reference.words == initial.words) {
    std::fprintf(stderr, "FAIL: horizontal fixture made no changes\n");
    std::exit(1);
  }
}

void set_sample(Image &frame, bool horizontal, int along, int across, uint value) {
  if (horizontal) {
    frame.set(across, along, value);
  } else {
    frame.set(along, across, value);
  }
}

// A row or column of blocks per level, with all four lengths in each 64-sample strip.
// Include enabled internal strip edges and sample cases with flat, partial and failed masks.
void filter_thresholds(int depth, int sharpness, bool horizontal) {
  constexpr int length = 189, breadth = 64 * 4;
  Image frame(horizontal ? breadth : length, horizontal ? length : breadth, depth);
  EdgeLists edges((frame.width + 3) / 4, (frame.height + 3) / 4, horizontal);
  const int scale = 1 << (depth - 8);
  const int units = (length + 3) / 4;
  for (int level = 0; level < 64; ++level) {
    for (int strip = 0; strip * kStrip < units; ++strip) {
      const uint block = edges.block_id(horizontal, strip, level);
      const int limit = std::min(kStrip, units - strip * kStrip);
      for (int pos = 0; pos < limit; pos += (pos & 3) == 2 ? 2 : 1) {
        const int type = pos / 4;
        uint enabled = (pos & 3) == 1 || (strip > 0 && pos == 0);
        edges.set(block, pos, (pos & 3) == 2 ? 2 : 1, type, enabled ? level : 0);
      }
    }
    for (int across = 0; across < 4; ++across) {
      for (int along = 0; along < units * 4; ++along) {
        const int rel = (along + 4) % 16;
        const int base = (level * 29 + (along + 4) / 16 * 3) % 211 + 20;
        int value = base * scale + (rel >= 8 ? 3 * scale : 0);
        if (across == 1 && (rel == 3 || rel == 13)) value += 2 * scale;
        if (across == 2 && (rel == 5 || rel == 10)) value += 2 * scale;
        if (across == 3) value += int(rng() % (12 * scale)) - 6 * scale;
        set_sample(frame, horizontal, along, level * 4 + across, value);
      }
    }
  }
  if (horizontal) {
    compare_horizontal(frame, edges, sharpness, sharpness % 3, sharpness % 3);
  } else {
    compare_vertical(frame, edges, sharpness, sharpness % 3);
  }
}

// Mix transform walks, zero levels, holes, tail lanes and partial blocks. Each run also
// has two nearby enabled edges within a strip, including a case with an in-place dependency.
void vertical_random(int depth, int trial) {
  Image frame(129 + rng() % 95, 61 + rng() % 37, depth);
  const int mi_cols = (frame.width + 3) / 4;
  const int mi_rows = (frame.height + 3) / 4;
  EdgeLists edges(mi_cols, mi_rows, false);
  const int scale = 1 << (depth - 8);
  for (int y = 0; y < mi_rows * 4; ++y) {
    for (int x = 0; x < mi_cols * 4; ++x) {
      const int base = 40 + (x / 4 + y / 4 * 11) % 160;
      const uint sample = trial % 4 == 0 ? rng() % (256 * scale) : base * scale + rng() % (3 * scale);
      frame.set(x, y, sample);
    }
  }

  for (int row = 0; row < mi_rows; ++row) {
    for (int bx = 0; bx < edges.block_cols; ++bx) {
      const int limit = std::min(kStrip, mi_cols - bx * kStrip);
      const int stop = trial % 5 == 0 ? rng() % (limit + 1) : limit;
      for (int pos = 0; pos < stop;) {
        int step = 1 << (rng() % 5);
        while (pos & (step - 1)) step >>= 1;
        const uint level = (bx == 0 && pos == 0) || rng() % 4 == 0 ? 0 : 1 + rng() % 63;
        edges.set(row * edges.block_cols + bx, pos, step, rng() % 4, level);
        pos += step;
      }
    }
  }

  // The unfiltered frame remains an effective oracle on the busiest random-noise trials
  // because this strip contains deliberately smooth adjacent edges.
  for (int x = 0; x < 16; ++x) {
    for (int y = 0; y < 4; ++y) frame.set(x, y, (100 + x / 4 * 7) * scale);
  }
  std::fill_n(edges.words.begin() + kListBase * 8, 8, 0);
  edges.set(0, 0, 1, 0, 0);
  edges.set(0, 1, 1, 1, 31);
  edges.set(0, 2, 1, 2, 63);
  compare_vertical(frame, edges, trial % 8, trial % 3);
}
// Adjacent edges are limited by the smaller transform on either side. Use that rule
// across vertical strip boundaries as well, so whole-plane dispatch has no cross-strip
// pixel dependency regardless of the order in which thread groups run.
void horizontal_random(int depth, int trial) {
  Image frame(65 + rng() % 127, 129 + rng() % 109, depth);
  const int mi_cols = (frame.width + 3) / 4, mi_rows = (frame.height + 3) / 4;
  const int scale = 1 << (depth - 8);
  EdgeLists edges(mi_cols, mi_rows, true);
  const uint plane = trial % 3;

  for (int x = 0; x < mi_cols * 4; ++x) {
    for (int y = 0; y < mi_rows * 4; ++y) {
      const int base = 70 + (x / 4 * 7 + y / 4) % 110;
      const uint sample = trial % 4 == 0 ? rng() % (256 * scale) : base * scale + rng() % (3 * scale);
      frame.set(x, y, sample);
    }
  }

  for (int col = 0; col < edges.block_cols; ++col) {
    int prev_step = 0;
    for (int by = 0; by < edges.block_rows; ++by) {
      const int end = std::min(kStrip, mi_rows - by * kStrip);
      const int stop = trial % 5 == 0 ? rng() % (end + 1) : end;
      for (int pos = 0; pos < stop;) {
        int step = 1 << (rng() % 5);
        while (pos & (step - 1)) step >>= 1;
        const uint level = prev_step == 0 || rng() % 4 == 0 ? 0 : 1 + rng() % 63;
        const int size = std::min(prev_step, step);
        const int type = size <= 1 ? 0 : plane ? 1 : size == 2 ? 2 : 3;
        edges.set(edges.block_id(true, by, col), pos, step, type, level);
        prev_step = step;
        pos += step;
      }
    }
  }

  // Guarantee that a small near-flat patch changes even on trials with a noisy picture.
  const uint block = edges.block_id(true, 0, 0);
  std::fill_n(edges.words.begin() + (kListBase + block) * 8, 8, 0);
  edges.set(block, 0, 1, 0, 0);
  edges.set(block, 1, 1, 0, 31);
  edges.set(block, 2, 1, 0, 63);
  for (int x = 0; x < 4; ++x) {
    for (int y = 0; y < 16; ++y) frame.set(x, y, (100 + y / 4 * 3) * scale);
  }
  compare_horizontal(frame, edges, trial % 8, plane, trial % 3);
}

#include "gen_check.h"
}  // namespace

int main() {
  gs_state.track_unwritten = true;
  check_generators();
  for (int depth : {8, 10}) {
    for (bool horizontal : {false, true}) {
      for (int sharpness = 0; sharpness < 8; ++sharpness) filter_thresholds(depth, sharpness, horizontal);
    }
    for (int trial = 0; trial < 48; ++trial) {
      vertical_random(depth, trial);
      horizontal_random(depth, trial);
    }
    std::printf("PASS: %d-bit vertical even/odd and all horizontal schedules, all levels, sharpness and lengths\n", depth);
    std::puts("PASS: 48 randomized frames per direction with complete-word ownership");
  }
}
