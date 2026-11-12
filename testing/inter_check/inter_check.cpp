/*
 * Check of the fast inter prediction shaders (test flag -interfast) against the original shaders and
 * against libaom's C code.
 *
 * With -interfast the blocks with one reference and the averaged compound blocks are listed by the
 * fraction of their motion vectors and predicted by inter_fast.hlsl and inter_compound_fast.hlsl, which
 * leave out the filter passes a vector without a fraction does not need (libav1/dx/shaders/
 * inter_fast_common.h). Here the shaders are compiled as C++ (run.sh) and run on generated frames laid
 * out like the decoder's frame pool: block records as gen_pred_blocks.hlsl writes them, constants as
 * prediction.cpp sets them, reference frames with the border inter_ext_borders extends. Every frame is
 * predicted three times:
 *
 *   - by libaom's C code of this tree (testing/warp_check/ref_glue.c: av1_make_inter_predictor);
 *   - by the original shaders, inter_main and inter_compound, from lists by block size;
 *   - the way -interfast runs it: lists by class and block size, the class from inter_fast_class() of
 *     gen_pred_blocks.hlsl, inter_fast and inter_compound_fast for all classes but the last, the
 *     original shaders for that one.
 *
 * The three pools, padding and reference frames included, must be byte identical. The cases cover every
 * block size, 8 and 10 bits, all fractions of luma and chroma vectors with every pair of filters, both
 * references of a compound block in every pair of classes, distance weights, residuals, sample values at
 * both ends of the range, and vectors that point far outside the frame on every side.
 *
 * Runs on a Linux host, no GPU:  sh testing/inter_check/run.sh
 */
#define GROUP_SCHED_IMPLEMENTATION
#include <stdio.h>

#include <algorithm>
#include <map>
#include <string>
#include <tuple>
#include <vector>

#include "inter_ops.h"

#include "../warp_check/ref_glue.h"
#include "../warp_check/shader_api.h"

long g_out_of_bounds = 0;
int g_double_model = 1;
long g_double_ops = 0;

extern const ShaderApi inter_main_api, inter_main_hbd_api, inter_compound_api, inter_compound_hbd_api;
extern const ShaderApi inter_fast_api, inter_fast_hbd_api, inter_compound_fast_api, inter_compound_fast_hbd_api;
extern const ShaderApi inter_ext_borders_api, inter_ext_borders_hbd_api;

// inter_fast_class() of gen_pred_blocks.hlsl (run.sh cuts it out of the shader): the class of a vector as
// the block record has it.
#include "inter_fast_class.gen.h"

// Dispatch((wi_count + 63) >> 6, 1, 1). These shaders have no group memory and no group sync: a thread
// reads the reference frames and its block record and writes its own samples.
static void dispatch(const ShaderApi *api, uint32_t wi_count) {
  const uint32_t threads = ((wi_count + 63) >> 6) << 6;
  for (uint32_t thread = 0; thread < threads; thread++) api->run(thread);
}

// ---------------------------------------------------------------------------
// Random numbers.
// ---------------------------------------------------------------------------
static uint64_t g_rng = 0x2545f4914f6cdd1dull;
static uint64_t rnd64() {
  g_rng ^= g_rng << 13;
  g_rng ^= g_rng >> 7;
  g_rng ^= g_rng << 17;
  return g_rng;
}
static int rnd(int n) { return static_cast<int>((rnd64() >> 20) % static_cast<uint64_t>(n)); }
static int rnd_range(int lo, int hi) { return lo + rnd(hi - lo + 1); }
static void fill_garbage(uint8_t *p, size_t size) {
  size_t i = 0;
  for (; i + 8 <= size; i += 8) {
    const uint64_t v = rnd64();
    memcpy(p + i, &v, 8);
  }
  for (; i < size; i++) p[i] = static_cast<uint8_t>(rnd64());
}

// ---------------------------------------------------------------------------
// The frame pool, laid out like av1_reallocate_frame_buffer() of av1_core.cpp (as in warp_check.cpp).
// ---------------------------------------------------------------------------
enum {
  kGuard = 4096,    // bytes of garbage before the first and after the last frame
  kRefBorder = 448  // the reference planes libaom reads are extended this far
};

struct Layout {
  int w, h, hbd, bpp;
  int y_stride, uv_stride;  // in samples
  size_t y_size, uv_size, fb_size;
  size_t base;  // of the frame's buffer in the pool
  int res_stride[3], res_offset[3];
  size_t res_size;
  int mi_cols, mi_rows;
  int plane_w(int p) const { return p ? (w + 1) >> 1 : w; }
  int plane_h(int p) const { return p ? (h + 1) >> 1 : h; }
  int stride(int p) const { return (p ? uv_stride : y_stride) * bpp; }  // in bytes
  int offset(int p) const {
    const size_t off = p == 0 ? base + 16 * bpp : base + (y_size + 16) * bpp + (p == 2 ? uv_size * bpp : 0);
    return static_cast<int>(off);
  }
};

static Layout make_layout(int w, int h, int hbd) {
  Layout l = {};
  l.w = w;
  l.h = h;
  l.hbd = hbd;
  l.bpp = hbd + 1;
  const int h_border = 16;
  const int aligned_width = (w + 127) & ~127;
  const int aligned_height = (h + 127) & ~127;
  l.y_stride = aligned_width + 2 * h_border;
  l.uv_stride = (aligned_width >> 1) + 2 * h_border;
  l.y_size = static_cast<size_t>(aligned_height) * l.y_stride;
  l.uv_size = static_cast<size_t>(aligned_height >> 1) * l.uv_stride;
  l.fb_size = (l.y_size + 2 * l.uv_size) * (1 + hbd);
  const int y_width = (w + 7) & ~7;
  const int y_height = (h + 7) & ~7;
  const int uv_width = y_width >> 1;
  const int uv_height = y_height >> 1;
  l.res_stride[0] = 2 * ((y_width + 127) & ~127);
  l.res_stride[1] = l.res_stride[2] = 2 * ((uv_width + 127) & ~127);
  l.res_offset[0] = 0;
  l.res_offset[1] = l.res_stride[0] * ((y_height + 127) & ~127);
  l.res_offset[2] = l.res_offset[1] + l.res_stride[1] * ((uv_height + 127) & ~127);
  l.res_size = static_cast<size_t>(l.res_offset[2]) + static_cast<size_t>(l.res_stride[2]) * ((uv_height + 127) & ~127);
  l.mi_cols = y_width >> 2;
  l.mi_rows = y_height >> 2;
  return l;
}

// ---------------------------------------------------------------------------
// Blocks.
// ---------------------------------------------------------------------------
struct Block {
  int mi_row, mi_col;
  int bw_log, bh_log;  // log2 of the luma size in units of 4 samples
  int refs;            // 1 or 2
  int ref[2];          // 0..6
  int mv[2][2];        // row, col in 1/8 luma sample
  int comp_type;       // REF_COMP_AVERAGE or REF_COMP_DISTANCE
  int fwd, bck;
  int filter_x, filter_y;  // 0 regular, 1 smooth, 2 sharp, 3 bilinear
  int noskip;
  int luma_w() const { return 4 << bw_log; }
  int luma_h() const { return 4 << bh_log; }
  // The chroma of a block with a side of 4 goes through the 2x2 shaders, which -interfast leaves alone.
  int has_chroma() const { return bw_log >= 1 && bh_log >= 1; }
};

struct Case {
  std::string name;
  Layout l[3];  // the frame that is predicted and two reference frames of its size
  int bit_depth;
  int content;
  int ref_frame[7];  // which of the two reference frames of the pool a reference is: 1 or 2
  std::vector<Block> blocks;
};

static const char *const kClassNames[4] = {"copy", "h", "v", "2d"};

static std::string describe(const Block &b, int plane) {
  static const char *const filters[] = {"regular", "smooth", "sharp", "bilinear"};
  char text[400];
  const int ss = plane > 0;
  int n = snprintf(text, sizeof(text), "%dx%d block at x %d y %d, plane %d, filters x %s y %s", b.luma_w(), b.luma_h(),
                   b.mi_col * 4, b.mi_row * 4, plane, filters[b.filter_x], filters[b.filter_y]);
  for (int r = 0; r < b.refs; r++) {
    const uint32_t word = (static_cast<uint32_t>(b.mv[r][0]) & 0xffff) | (static_cast<uint32_t>(b.mv[r][1]) << 16);
    n += snprintf(text + n, sizeof(text) - n, ", ref %d mv row %d col %d (%s)", b.ref[r] + 1, b.mv[r][0], b.mv[r][1],
                  kClassNames[inter_fast_class(ss ? word : (word << 1) & 0xfffeffffu)]);
  }
  if (b.refs == 2)
    n += snprintf(text + n, sizeof(text) - n, ", weights %d %d", b.comp_type == REF_COMP_DISTANCE ? b.fwd : 8,
                  b.comp_type == REF_COMP_DISTANCE ? b.bck : 8);
  snprintf(text + n, sizeof(text) - n, "%s", b.noskip ? ", with residual" : "");
  return text;
}

// (w_log, h_log) of the inter block sizes: InterBlockSizeIndexLUT of av1_compute.h.
static const int kSizes[22][2] = {{0, 0}, {0, 1}, {0, 2}, {1, 0}, {1, 1}, {1, 2}, {1, 3}, {2, 0}, {2, 1}, {2, 2}, {2, 3},
                                  {2, 4}, {3, 1}, {3, 2}, {3, 3}, {3, 4}, {4, 2}, {4, 3}, {4, 4}, {4, 5}, {5, 4}, {5, 5}};

// One component of a motion vector, in 1/8 luma sample. |kind|: 0 a whole number of chroma samples (a
// multiple of 16: no fraction in either plane), 1 a whole number of luma samples that is half a chroma
// sample, 2 a fraction in both planes. |reach|: how far it goes, in luma samples.
static int mv_component(int kind, int reach) {
  const int whole = rnd_range(-reach, reach);
  if (kind == 0) return (whole & ~1) * 8;
  if (kind == 1) return (whole | 1) * 8;
  return whole * 8 + rnd_range(1, 7);
}

// A vector whose class is |cls| in luma (|by_chroma| 0) or in chroma (1); the other plane gets what follows.
static void class_mv(int *mv, int cls, int by_chroma, int reach) {
  for (int d = 0; d < 2; d++) {
    const int fraction = (cls >> (1 - d)) & 1;  // mv[0] is the row: bit 1 of the class
    int kind;
    if (by_chroma)
      kind = fraction ? rnd_range(1, 2) : 0;
    else
      kind = fraction ? 2 : rnd(2);
    mv[d] = mv_component(kind, reach);
  }
}

static void random_block_modes(Block *b, int allow_compound) {
  b->refs = allow_compound && b->bw_log >= 1 && b->bh_log >= 1 && rnd(2) ? 2 : 1;
  b->ref[0] = rnd(7);
  b->ref[1] = rnd(7);
  b->filter_x = rnd(4);
  b->filter_y = rnd(3) ? b->filter_x : rnd(4);
  b->noskip = rnd(3) == 0;
  b->comp_type = rnd(2) ? REF_COMP_DISTANCE : REF_COMP_AVERAGE;
  b->fwd = b->bck = 8;
  if (b->comp_type == REF_COMP_DISTANCE) ref_distance_weights(rnd(4), rnd(2), &b->fwd, &b->bck);
}

static void set_layouts(Case *c, int w, int h, int hbd) {
  size_t slot = 0;
  for (int frame = 0; frame < 3; frame++) {
    c->l[frame] = make_layout(w, h, hbd);
    slot = c->l[frame].fb_size;
  }
  for (int frame = 0; frame < 3; frame++) c->l[frame].base = kGuard + static_cast<size_t>(frame) * slot;
  c->bit_depth = hbd ? 10 : 8;
  for (int r = 0; r < 7; r++) c->ref_frame[r] = 1 + rnd(2);
}

// Blocks of random sizes and modes, placed where there is room.
static void random_case(Case *c, int w, int h, int hbd) {
  set_layouts(c, w, h, hbd);
  c->content = rnd(5);
  const int grid_w = c->l[0].mi_cols, grid_h = c->l[0].mi_rows;
  std::vector<uint8_t> used(static_cast<size_t>(grid_w) * grid_h);
  const int reach = rnd(3) ? 24 : 160;
  for (int tries = 0; tries < 4 * grid_w * grid_h; tries++) {
    Block b = {};
    const int *size = kSizes[rnd(22)];
    b.bw_log = size[0];
    b.bh_log = size[1];
    const int bw = 1 << b.bw_log, bh = 1 << b.bh_log;
    if (bw > grid_w || bh > grid_h) continue;
    b.mi_col = rnd((grid_w - bw) / bw + 1) * bw;
    b.mi_row = rnd((grid_h - bh) / bh + 1) * bh;
    bool free = true;
    for (int y = 0; y < bh && free; y++)
      for (int x = 0; x < bw && free; x++) free = !used[static_cast<size_t>(b.mi_row + y) * grid_w + b.mi_col + x];
    if (!free) continue;
    random_block_modes(&b, 1);
    for (int r = 0; r < 2; r++) class_mv(b.mv[r], rnd(4), rnd(2), reach);
    for (int y = 0; y < bh; y++)
      for (int x = 0; x < bw; x++) used[static_cast<size_t>(b.mi_row + y) * grid_w + b.mi_col + x] = 1;
    c->blocks.push_back(b);
  }
}

// A frame of 5 x 4 blocks of one size: one block per class with one reference, and one per pair of
// classes with two (a size that cannot have two references: the four classes five times). The classes are
// those of luma or those of chroma.
static void coverage_case(Case *c, const int *size, int hbd, int by_chroma) {
  const int bw = 1 << size[0], bh = 1 << size[1];
  set_layouts(c, 5 * 4 * bw, 4 * 4 * bh, hbd);
  c->content = rnd(5);
  const int compound_size = size[0] >= 1 && size[1] >= 1;
  for (int k = 0; k < 20; k++) {
    Block b = {};
    b.bw_log = size[0];
    b.bh_log = size[1];
    b.mi_col = (k % 5) * bw;
    b.mi_row = (k / 5) * bh;
    random_block_modes(&b, 0);
    const int reach = rnd(2) ? 12 : 4 * std::max(bw, bh) + 40;
    if (compound_size && k >= 4) {
      b.refs = 2;
      class_mv(b.mv[0], (k - 4) & 3, by_chroma, reach);
      class_mv(b.mv[1], (k - 4) >> 2, by_chroma, reach);
    } else {
      class_mv(b.mv[0], k & 3, by_chroma, reach);
    }
    c->blocks.push_back(b);
  }
}

// A frame of 256 blocks of 8x8 with one pair of filters: the components of the vectors run through 0..15
// in 1/8 luma sample, which is every fraction of a luma vector (twice) and of a chroma vector, in every
// combination of row and column. The second reference of the compound blocks has another combination.
static void sweep_case(Case *c, int hbd, int filter_x, int filter_y, int compound) {
  set_layouts(c, 128, 128, hbd);
  c->content = rnd(5);
  for (int k = 0; k < 256; k++) {
    Block b = {};
    b.bw_log = b.bh_log = 1;
    b.mi_col = (k & 15) * 2;
    b.mi_row = (k >> 4) * 2;
    random_block_modes(&b, 0);
    b.refs = 1 + compound;
    b.filter_x = filter_x;
    b.filter_y = filter_y;
    const int k1 = (k * 37 + 101) & 255;
    b.mv[0][0] = (k >> 4) + 16 * rnd_range(-3, 3);
    b.mv[0][1] = (k & 15) + 16 * rnd_range(-3, 3);
    b.mv[1][0] = (k1 >> 4) + 16 * rnd_range(-3, 3);
    b.mv[1][1] = (k1 & 15) + 16 * rnd_range(-3, 3);
    c->blocks.push_back(b);
  }
}

// ---------------------------------------------------------------------------
// One case: the pool, the block records, libaom, the original shaders, the fast path, the comparison.
// ---------------------------------------------------------------------------
static int g_kernels[8 * 16][8];

struct Stats {
  long cases = 0, blocks = 0, records = 0;
  long mismatches[2] = {0, 0};  // cases in which the original shaders, the fast path differ from libaom
  long shown = 0;
  // Block records by plane kind (luma, chroma), class (one reference: 0..3; two: 4 + pair) and list size.
  std::map<std::tuple<int, int, int, int>, long> coverage;
  long fast_records = 0, original_records = 0;  // records of the fast path by the shader that ran them
};

static void put_sample(uint8_t *p, int hbd, int v) {
  if (hbd) {
    const uint16_t s = static_cast<uint16_t>(v);
    memcpy(p, &s, 2);
  } else {
    *p = static_cast<uint8_t>(v);
  }
}

static int picture_sample(const Case &c, int frame, int p, int x, int y, int max) {
  const int w = c.l[frame].plane_w(p), h = c.l[frame].plane_h(p);
  switch (c.content) {
    case 0:
      return rnd(max + 1);
    case 1:
      return std::min(
          max, std::max(0, (x * 5 + y * 3 + frame * 37 + p * 91) * max / (5 * w + 3 * h + 200) + rnd_range(-6, 6)));
    case 2:
      // Flat areas with single samples at either end of the range: the largest steps a filter can see.
      return rnd(6) ? (max * (3 + frame + p)) >> 3 : rnd(2) ? max : 0;
    case 3:
      return ((x >> 1) + (y >> 1) + frame) & 1 ? max : 0;
    default:
      return (x + y + frame) & 1 ? max : 0;
  }
}

// The InterFilterLut of gen_pred_blocks.hlsl: the kernel table for a filter and a block side.
static uint32_t filter_type(int filter, int side_log) {
  return (0x25432010u >> (((filter & 15) << 2) + ((side_log > 0) << 4))) & 7;
}

static uint32_t mv_word(const int *mv) {
  return (static_cast<uint32_t>(mv[0]) & 0xffff) | (static_cast<uint32_t>(mv[1]) << 16);
}

struct List {
  std::vector<uint32_t> words;  // the records of one block list
  int count = 0;
};

struct Pass {
  const ShaderApi *api;
  uint32_t srt[4];
};

static bool run_case(const Case &c, Stats *stats, int verbose) {
  const Layout &l = c.l[0];
  const int hbd = l.hbd, bpp = l.bpp, max = (1 << c.bit_depth) - 1;
  const size_t pool_size = kGuard * 2 + l.fb_size * 3;

  // The pool: all garbage, then the pictures of the two reference frames. What lies beyond a picture inside
  // its buffer stays garbage, as nothing in the decoder defines it, except the border the decoder extends.
  std::vector<uint8_t> pool(pool_size);
  fill_garbage(pool.data(), pool.size());
  std::vector<std::vector<uint16_t>> padded16[3];
  std::vector<std::vector<uint8_t>> padded8[3];
  for (int frame = 1; frame <= 2; frame++) {
    padded16[frame].resize(3);
    padded8[frame].resize(3);
    const Layout &fl = c.l[frame];
    for (int p = 0; p < 3; p++) {
      const int w = fl.plane_w(p), h = fl.plane_h(p);
      const int pw = w + 2 * kRefBorder, ph = h + 2 * kRefBorder;
      std::vector<uint16_t> picture(static_cast<size_t>(w) * h);
      for (int y = 0; y < h; y++)
        for (int x = 0; x < w; x++) {
          const int v = picture_sample(c, frame, p, x, y, max);
          picture[static_cast<size_t>(y) * w + x] = static_cast<uint16_t>(v);
          put_sample(&pool[fl.offset(p) + static_cast<size_t>(y) * fl.stride(p) + static_cast<size_t>(x) * bpp], hbd,
                     v);
        }
      // What libaom reads: the plane with its edges repeated.
      if (hbd)
        padded16[frame][p].resize(static_cast<size_t>(pw) * ph);
      else
        padded8[frame][p].resize(static_cast<size_t>(pw) * ph);
      for (int y = 0; y < ph; y++)
        for (int x = 0; x < pw; x++) {
          const int sx = std::min(w - 1, std::max(0, x - kRefBorder)),
                    sy = std::min(h - 1, std::max(0, y - kRefBorder));
          const uint16_t v = picture[static_cast<size_t>(sy) * w + sx];
          if (hbd)
            padded16[frame][p][static_cast<size_t>(y) * pw + x] = v;
          else
            padded8[frame][p][static_cast<size_t>(y) * pw + x] = static_cast<uint8_t>(v);
        }
    }
  }

  std::vector<uint8_t> residuals(l.res_size);
  {
    const int range = rnd(4) ? 48 : 2 * max;
    for (size_t i = 0; i + 2 <= residuals.size(); i += 2) {
      const int16_t r = static_cast<int16_t>(rnd_range(-range, range));
      memcpy(&residuals[i], &r, 2);
    }
  }

  // Block records, as gen_pred_blocks.hlsl writes them: into the lists by type and block size of the
  // original path, and into the lists by class and block size of gen_pred_blocks_fast.hlsl.
  // Keys: (two references, width, height) and (two references, class or pair of classes, width, height).
  std::map<std::tuple<int, int, int>, List> lists;
  std::map<std::tuple<int, int, int, int>, List> fast_lists;
  for (const Block &b : c.blocks) {
    const uint32_t noskip = b.noskip ? 0x2000u : 0;
    const int compound = b.refs == 2;
    for (int p = 0; p < (b.has_chroma() ? 3 : 1); p++) {
      const int ss = p > 0;
      const int w_log = b.bw_log - ss, h_log = b.bh_log - ss;
      uint32_t words[4];
      words[0] = static_cast<uint32_t>(b.mi_col >> ss) | (static_cast<uint32_t>(b.mi_row >> ss) << 16);
      words[1] = static_cast<uint32_t>(p) | (static_cast<uint32_t>(b.ref[0]) << 2) |
                 (filter_type(b.filter_x, w_log) << 5) | (filter_type(b.filter_y, h_log) << 9) | noskip;
      // Luma vectors are stored doubled (1/16 sample), chroma vectors as they are.
      words[2] = ss ? mv_word(b.mv[0]) : (mv_word(b.mv[0]) << 1) & 0xfffeffffu;
      words[3] = 0;
      int cls = inter_fast_class(words[2]);
      if (compound) {
        const uint32_t wtd = b.comp_type == REF_COMP_DISTANCE ? static_cast<uint32_t>(b.fwd | (b.bck << 4)) : 0x88;
        words[1] |= (static_cast<uint32_t>(b.ref[1]) << 14) | (wtd << 17);  // gpu_comp_type 0
        words[3] = ss ? mv_word(b.mv[1]) : (mv_word(b.mv[1]) << 1) & 0xfffeffffu;
        cls |= inter_fast_class(words[3]) << 2;
      }
      List &list = lists[std::make_tuple(compound, w_log, h_log)];
      List &fast = fast_lists[std::make_tuple(compound, cls, w_log, h_log)];
      for (int i = 0; i < 4; i++) {
        list.words.push_back(words[i]);
        fast.words.push_back(words[i]);
      }
      list.count++;
      fast.count++;
      stats->records++;
      stats->coverage[std::make_tuple(ss, compound ? 4 + cls : cls, w_log, h_log)]++;
    }
  }
  // The passes of the two paths, as av1_prediction_run_all() dispatches them: a thread of a compound shader
  // predicts 4x2 samples, so its lists are one unit higher.
  std::vector<uint32_t> pred_words;
  std::vector<Pass> passes[2];
  for (auto &entry : lists) {
    const int compound = std::get<0>(entry.first), w = std::get<1>(entry.first), h = std::get<2>(entry.first) + compound;
    const ShaderApi *api = compound ? (hbd ? &inter_compound_hbd_api : &inter_compound_api)
                                    : (hbd ? &inter_main_hbd_api : &inter_main_api);
    passes[0].push_back({api,
                         {static_cast<uint32_t>((1 << (w + h)) * entry.second.count),
                          static_cast<uint32_t>(pred_words.size() / 4), static_cast<uint32_t>(w),
                          static_cast<uint32_t>(h)}});
    pred_words.insert(pred_words.end(), entry.second.words.begin(), entry.second.words.end());
  }
  for (auto &entry : fast_lists) {
    const int compound = std::get<0>(entry.first), cls = std::get<1>(entry.first);
    const int w = std::get<2>(entry.first), h = std::get<3>(entry.first) + compound;
    const int original = cls == (compound ? 15 : 3);
    const ShaderApi *api;
    if (original)
      api = compound ? (hbd ? &inter_compound_hbd_api : &inter_compound_api)
                     : (hbd ? &inter_main_hbd_api : &inter_main_api);
    else
      api = compound ? (hbd ? &inter_compound_fast_hbd_api : &inter_compound_fast_api)
                     : (hbd ? &inter_fast_hbd_api : &inter_fast_api);
    (original ? stats->original_records : stats->fast_records) += entry.second.count;
    passes[1].push_back({api,
                         {static_cast<uint32_t>((1 << (w + h)) * entry.second.count),
                          static_cast<uint32_t>(pred_words.size() / 4),
                          static_cast<uint32_t>(original ? w : w | (cls << 8)), static_cast<uint32_t>(h)}});
    pred_words.insert(pred_words.end(), entry.second.words.begin(), entry.second.words.end());
  }
  // The lists are followed by garbage: a thread that looks past its list must not get a valid block.
  for (int i = 0; i < 64; i++) pred_words.push_back(static_cast<uint32_t>(rnd64()));

  ShaderBuffers buffers = {};
  buffers.pred_blocks = reinterpret_cast<const uint8_t *>(pred_words.data());
  buffers.pred_blocks_size = pred_words.size() * 4;
  buffers.residuals = residuals.data();
  buffers.residuals_size = residuals.size();
  buffers.frame = pool.data();
  buffers.frame_size = pool.size();

  // The border of the reference frames, extended by the decoder's own shader when a frame is done.
  const ShaderApi *border_api = hbd ? &inter_ext_borders_hbd_api : &inter_ext_borders_api;
  for (int frame = 1; frame <= 2; frame++) {
    const Layout &fl = c.l[frame];
    BorderConsts bc = {};
    for (int p = 0; p < 3; p++) {
      bc.planes[p][0] = fl.stride(p);
      bc.planes[p][1] = fl.offset(p);
    }
    bc.dims[0][0] = fl.plane_w(0);
    bc.dims[0][1] = fl.plane_h(0);
    bc.dims[1][0] = fl.plane_w(1);
    bc.dims[1][1] = fl.plane_h(1);
    border_api->bind(&buffers);
    border_api->set_common(&bc);
    dispatch(border_api, static_cast<uint32_t>(fl.plane_h(0) + 2 * fl.plane_h(1)));
  }

  // PSSLInterData, as av1_prediction_run_all() fills it. What these shaders do not read is garbage here.
  static InterConsts consts;
  fill_garbage(reinterpret_cast<uint8_t *>(&consts), sizeof(consts));
  for (int p = 0; p < 3; p++) {
    consts.planes[p][0] = l.stride(p);
    consts.planes[p][1] = l.offset(p);
    consts.planes[p][2] = l.res_stride[p];
    consts.planes[p][3] = l.res_offset[p];
  }
  for (int r = 0; r < 7; r++)
    for (int p = 0; p < 3; p++) {
      const Layout &fl = c.l[c.ref_frame[r]];
      int *rp = consts.refplanes[r * 3 + p];
      rp[0] = fl.stride(p);
      rp[1] = fl.offset(p);
      rp[2] = fl.plane_w(p) - 1;
      rp[3] = fl.plane_h(p) - 1;
    }
  consts.dims[0][0] = consts.refplanes[0][2];
  consts.dims[0][1] = consts.refplanes[0][3];
  consts.dims[1][0] = consts.refplanes[1][2];
  consts.dims[1][1] = consts.refplanes[1][3];
  memcpy(consts.kernels, g_kernels, sizeof(consts.kernels));

  // What libaom makes of the same blocks.
  std::vector<uint8_t> expected(pool);
  std::vector<uint16_t> block16(128 * 128);
  std::vector<uint8_t> block8(128 * 128);
  for (const Block &b : c.blocks) {
    for (int p = 0; p < (b.has_chroma() ? 3 : 1); p++) {
      const int ss = p > 0;
      RefBlock rb = {};
      rb.bit_depth = c.bit_depth;
      rb.plane = p;
      rb.luma_w = b.luma_w();
      rb.luma_h = b.luma_h();
      rb.mi_row = b.mi_row;
      rb.mi_col = b.mi_col;
      rb.refs = b.refs;
      for (int r = 0; r < rb.refs; r++) {
        RefSide &s = rb.side[r];
        const int frame = c.ref_frame[b.ref[r]];
        const Layout &fl = c.l[frame];
        const int pw = fl.plane_w(p) + 2 * kRefBorder;
        const size_t origin = static_cast<size_t>(kRefBorder) * pw + kRefBorder;
        s.origin = hbd ? static_cast<const void *>(&padded16[frame][p][origin])
                       : static_cast<const void *>(&padded8[frame][p][origin]);
        s.stride = pw;
        s.width = fl.plane_w(p);
        s.height = fl.plane_h(p);
        s.kind = REF_SIDE_MV;
        s.mat[2] = s.mat[5] = 1 << 16;
        s.mv_row = b.mv[r][0];
        s.mv_col = b.mv[r][1];
      }
      rb.filter_x = b.filter_x;
      rb.filter_y = b.filter_y;
      rb.comp_type = b.comp_type;
      rb.fwd_offset = b.fwd;
      rb.bck_offset = b.bck;
      rb.dst = hbd ? static_cast<void *>(block16.data()) : static_cast<void *>(block8.data());
      rb.dst_stride = 128;
      ref_predict(&rb);
      const int w = b.luma_w() >> ss, h = b.luma_h() >> ss;
      const int px = (b.mi_col * 4) >> ss, py = (b.mi_row * 4) >> ss;
      for (int y = 0; y < h; y++)
        for (int x = 0; x < w; x++) {
          int v = hbd ? block16[y * 128 + x] : block8[y * 128 + x];
          if (b.noskip) {
            int16_t r;
            memcpy(&r,
                   &residuals[static_cast<size_t>(l.res_offset[p]) + static_cast<size_t>(py + y) * l.res_stride[p] +
                              2 * (px + x)],
                   2);
            v = std::min(max, std::max(0, v + r));
          }
          put_sample(
              &expected[l.offset(p) + static_cast<size_t>(py + y) * l.stride(p) + static_cast<size_t>(px + x) * bpp],
              hbd, v);
        }
    }
  }

  bool ok = true;
  for (int path = 0; path < 2; path++) {
    std::vector<uint8_t> result(pool);
    buffers.frame = result.data();
    const long oob_before = g_out_of_bounds;
    for (const Pass &pass : passes[path]) {
      pass.api->bind(&buffers);
      pass.api->set_common(&consts);
      pass.api->set_srt(pass.srt);
      dispatch(pass.api, pass.srt[0]);
    }
    const bool same = memcmp(result.data(), expected.data(), pool_size) == 0;
    if (same && g_out_of_bounds == oob_before) continue;
    ok = false;
    stats->mismatches[path]++;
    if (stats->shown >= 12 && !verbose) continue;
    stats->shown++;
    printf("MISMATCH %s, %dx%d %d-bit, %s", c.name.c_str(), l.w, l.h, c.bit_depth,
           path ? "the fast path (-interfast)" : "the original shaders");
    if (g_out_of_bounds != oob_before) printf(", %ld accesses outside the buffers", g_out_of_bounds - oob_before);
    printf("\n");
    if (same) continue;
    size_t at = 0;
    while (result[at] == expected[at]) at++;
    long wrong = 0;
    for (size_t i = 0; i < pool_size; i++) wrong += result[i] != expected[i];
    bool located = false;
    for (int frame = 0; frame < 3 && !located; frame++)
      for (int p = 0; p < 3 && !located; p++) {
        const Layout &fl = c.l[frame];
        const long rel = static_cast<long>(at) - (fl.offset(p) - 16 * bpp);
        const long rows = static_cast<long>((p ? fl.uv_size : fl.y_size) / (p ? fl.uv_stride : fl.y_stride));
        if (rel < 0 || rel >= rows * fl.stride(p)) continue;
        located = true;
        const int y = static_cast<int>(rel / fl.stride(p)), x = static_cast<int>(rel % fl.stride(p)) / bpp - 16;
        int got = result[at & ~static_cast<size_t>(bpp - 1)], want = expected[at & ~static_cast<size_t>(bpp - 1)];
        if (hbd) {
          got |= result[(at & ~static_cast<size_t>(1)) + 1] << 8;
          want |= expected[(at & ~static_cast<size_t>(1)) + 1] << 8;
        }
        printf("  %ld bytes differ; first in %s, plane %d, x %d y %d: shader %d, libaom %d\n", wrong,
               frame ? "a reference frame" : "the frame", p, x, y, got, want);
        if (frame) continue;
        for (const Block &b : c.blocks) {
          const int ss = p > 0;
          const int bx = (b.mi_col * 4) >> ss, by = (b.mi_row * 4) >> ss;
          if (x >= bx && x < bx + (b.luma_w() >> ss) && y >= by && y < by + (b.luma_h() >> ss))
            printf("  in the %s\n", describe(b, p).c_str());
        }
      }
    if (!located) printf("  %ld bytes differ; first at pool offset %zu, outside the frames\n", wrong, at);
  }
  stats->cases++;
  stats->blocks += static_cast<long>(c.blocks.size());
  return ok;
}

// ---------------------------------------------------------------------------
// The class of a vector: what av1_mi_push_block() of prediction.cpp counts a block into must be the list
// gen_pred_blocks_fast.hlsl puts its record into.
// ---------------------------------------------------------------------------
// inter_fast_class() of prediction.cpp: from the vector as the mode info has it, |frac_mask| 7 for luma.
static int host_class(int row, int col, int frac_mask) { return ((col & frac_mask) ? 1 : 0) | ((row & frac_mask) ? 2 : 0); }

static long check_classes(long *checked) {
  long wrong = 0;
  for (int i = 0; i < 4 * 65536 + 400000; i++) {
    int row, col;
    if (i < 4 * 65536) {
      // Every value of one component next to four values of the other.
      static const int others[4] = {0, 8, -16, 5};
      row = (i & 0xffff) - 32768;
      col = others[i >> 16];
      if (i & 1) std::swap(row, col);
    } else {
      row = rnd_range(-32768, 32767);
      col = rnd_range(-32768, 32767);
    }
    const int mv[2] = {row, col};
    // Luma: the record has the vector doubled; chroma: as it is.
    const int luma = inter_fast_class((mv_word(mv) << 1) & 0xfffeffffu), chroma = inter_fast_class(mv_word(mv));
    (*checked) += 2;
    if (luma != host_class(row, col, 7) || chroma != host_class(row, col, 15)) {
      if (wrong++ < 3)
        printf("MISMATCH class of the vector row %d col %d: shader %d (luma) %d (chroma), host %d %d\n", row, col, luma,
               chroma, host_class(row, col, 7), host_class(row, col, 15));
    }
  }
  return wrong;
}

int main(int argc, char **argv) {
  int random_cases = 40, verbose = 0;
  for (int i = 1; i < argc; i++) {
    if (!strcmp(argv[i], "--cases") && i + 1 < argc) {
      random_cases = atoi(argv[++i]);
    } else if (!strcmp(argv[i], "--seed") && i + 1 < argc) {
      g_rng ^= strtoull(argv[++i], nullptr, 0) * 0x9e3779b97f4a7c15ull;
    } else if (!strcmp(argv[i], "-v")) {
      verbose = 1;
    } else {
      fprintf(stderr, "usage: inter_check [--cases N] [--seed S] [-v]\n");
      return 2;
    }
  }
  ref_init();
  ref_kernels(g_kernels);
  bool ok = true;

  // What the classes rest on: the kernel of fraction 0 only passes the sample on, for every filter type.
  for (int type = 0; type < 6; type++)
    for (int tap = 0; tap < 8; tap++)
      if (g_kernels[type * 16][tap] != (tap == 3 ? 128 : 0)) {
        printf("MISMATCH the kernel of fraction 0 of filter type %d is not {0, 0, 0, 128, 0, 0, 0, 0}\n", type);
        ok = false;
      }

  long class_values = 0;
  const long class_wrong = check_classes(&class_values);
  printf("inter_check: class of a motion vector, shader (block record) against host (mode info), %ld values: %s\n",
         class_values, class_wrong ? "DIFFERENT" : "identical");
  ok = ok && !class_wrong;

  Stats stats;
  char name[160];
  for (int hbd = 0; hbd < 2; hbd++) {
    // Every block size with every class and pair of classes, by luma and by chroma.
    for (int size = 0; size < 22; size++)
      for (int by_chroma = 0; by_chroma < 2; by_chroma++) {
        Case c;
        coverage_case(&c, kSizes[size], hbd, by_chroma);
        snprintf(name, sizeof(name), "%dx%d blocks, classes of %s", 4 << kSizes[size][0], 4 << kSizes[size][1],
                 by_chroma ? "chroma" : "luma");
        c.name = name;
        ok = run_case(c, &stats, verbose) && ok;
      }
    // Every fraction with every pair of filters.
    for (int filter_x = 0; filter_x < 4; filter_x++)
      for (int filter_y = 0; filter_y < 4; filter_y++)
        for (int compound = 0; compound < 2; compound++) {
          Case c;
          sweep_case(&c, hbd, filter_x, filter_y, compound);
          snprintf(name, sizeof(name), "all fractions, filters x %d y %d, %s", filter_x, filter_y,
                   compound ? "two references" : "one reference");
          c.name = name;
          ok = run_case(c, &stats, verbose) && ok;
        }
    // Frames of mixed blocks, small ones too: every block is near an edge there.
    static const int sizes[][2] = {{16, 16}, {24, 40}, {64, 64}, {72, 40}, {130, 98}, {200, 136}, {264, 200}, {36, 20}};
    for (int i = 0; i < random_cases; i++) {
      Case c;
      const int *wh = sizes[i % 8];
      random_case(&c, wh[0], wh[1], hbd);
      snprintf(name, sizeof(name), "mixed blocks %d", i);
      c.name = name;
      ok = run_case(c, &stats, verbose) && ok;
    }
  }

  // Every list the fast path can have was run: 22 block sizes with one reference in luma, and the 17 sizes
  // with both sides of 8 or more for chroma (one unit smaller) and for two references.
  long missing = 0;
  for (int ss = 0; ss < 2; ss++)
    for (int size = 0; size < 22; size++) {
      const int w = kSizes[size][0], h = kSizes[size][1];
      const int two_sided = w >= 1 && h >= 1;
      if (ss && !two_sided) continue;
      for (int cls = 0; cls < 20; cls++) {
        if (cls >= 4 && !two_sided) continue;
        if (!stats.coverage[std::make_tuple(ss, cls, w - ss, h - ss)]) {
          missing++;
          printf("MISSING %s list of %dx%d, class %d: no block\n", ss ? "chroma" : "luma", 4 << (w - ss), 4 << (h - ss),
                 cls);
        }
      }
    }
  ok = ok && !missing;

  printf("inter_check: %ld frames, %ld blocks, %ld block records (luma, U, V), 8 and 10 bits\n", stats.cases,
         stats.blocks, stats.records);
  printf("inter_check: fast path: %ld records through inter_fast and inter_compound_fast, %ld of the last class "
         "through the original shaders\n",
         stats.fast_records, stats.original_records);
  printf("inter_check: lists not covered: %ld (of 22 luma and 17 chroma sizes x 4 classes, and 17 + 17 sizes x 16 "
         "pairs of classes)\n",
         missing);
  printf("inter_check: frames that differ from libaom: %ld with the original shaders, %ld with the fast path\n",
         stats.mismatches[0], stats.mismatches[1]);
  if (g_out_of_bounds) printf("inter_check: %ld accesses outside the buffers\n", g_out_of_bounds);
  ok = ok && !g_out_of_bounds && !stats.mismatches[0] && !stats.mismatches[1];
  printf("inter_check: %s\n", ok ? "PASS" : "FAIL");
  return ok ? 0 : 1;
}
