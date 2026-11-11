/*
 * Check of the warp inter prediction shaders against libaom's C code.
 *
 * The shaders that predict warped blocks (inter_warp for one reference with a
 * local or a global warp, inter_scale_warp_compound for GLOBAL_GLOBALMV
 * blocks, and their _hbd builds) are compiled as C++ (run.sh, hlsl_shim.h)
 * and run on generated frames laid out like the decoder's frame pool: block
 * records as gen_pred_blocks.hlsl writes them, constants as prediction.cpp
 * sets them, reference frames with the border inter_ext_borders extends. The
 * same blocks are predicted by libaom's C code of this tree (ref_glue.c). The
 * whole pool, padding and neighbouring frames included, must be byte
 * identical.
 *
 * Every case runs twice: with shader double precision working, and with it
 * yielding 0 (hlsl_shim.h, g_double_model), which is how the console behaves.
 *
 * The threads of a group run under an adversarial schedule and the group
 * memory is watched for races (testing/common/group_sched.h): a race, a group
 * sync that not all threads reach, or a plain GroupMemoryBarrier() fails the
 * check, as a frame that differs does.
 *
 * Runs on a Linux host, no GPU:  sh testing/warp_check/run.sh
 */
#define GROUP_SCHED_IMPLEMENTATION
#include <stdio.h>

#include <algorithm>
#include <map>
#include <string>
#include <utility>
#include <vector>

#include "hlsl_shim.h"
#include "ref_glue.h"
#include "shader_api.h"

long g_out_of_bounds = 0;
int g_double_model = 1;
long g_double_ops = 0;

// ---------------------------------------------------------------------------
// Thread groups: testing/common/group_sched.h. Between two group syncs every thread runs alone, in an
// order drawn anew at every sync, and the group memory records every read of what another thread wrote
// since the last group sync: a shader passes only if it does not depend on how the threads of a group
// are interleaved (upstream's compound shader did: it counted on the 64 threads running in lockstep).
// ---------------------------------------------------------------------------
enum { kThreads = 64 };

static uint64_t g_scramble_state = 0x9e3779b97f4a7c15ull;
static uint64_t g_order_state = 0x2545f4914f6cdd1dull;

struct GroupContext {
  const ShaderApi *api;
  uint32_t first_thread;
};

static void thread_body(int thread, void *context) {
  const GroupContext *c = static_cast<const GroupContext *>(context);
  c->api->run(c->first_thread + static_cast<uint32_t>(thread));
}

static void run_group(const ShaderApi *api, uint32_t group) {
  GroupContext context = { api, group * kThreads };
  api->scramble(&g_scramble_state);
  gs_state.shader = api == &inter_warp_api            ? "inter_warp.hlsl"
                    : api == &inter_warp_hbd_api      ? "inter_warp_hbd.hlsl"
                    : api == &inter_warp_comp_api     ? "inter_scale_warp_compound.hlsl"
                    : api == &inter_warp_comp_hbd_api ? "inter_scale_warp_compound_hbd.hlsl"
                                                      : "inter_ext_borders.hlsl";
  gs_run_group(kThreads, thread_body, &context, &g_order_state);
}

// Dispatch((wi_count + 63) >> 6, 1, 1)
static void dispatch(const ShaderApi *api, uint32_t wi_count) {
  for (uint32_t group = 0; group < (wi_count + 63) >> 6; group++) run_group(api, group);
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
// The frame pool, laid out like av1_reallocate_frame_buffer() of av1_core.cpp.
// ---------------------------------------------------------------------------
enum {
  kGuard = 4096,    // bytes of garbage before the first and after the last frame
  kRefBorder = 320  // the reference planes libaom reads are extended this far
};

struct Layout {
  int w, h, hbd, bpp;
  int y_stride, uv_stride;  // in samples
  size_t y_size, uv_size, fb_size;
  int res_stride[3], res_offset[3];
  size_t res_size;
  int mi_cols, mi_rows;
  int plane_w(int p) const { return p ? (w + 1) >> 1 : w; }
  int plane_h(int p) const { return p ? (h + 1) >> 1 : h; }
  int stride(int p) const { return (p ? uv_stride : y_stride) * bpp; }  // in bytes
  int offset(int frame, int p) const {
    const size_t base = kGuard + static_cast<size_t>(frame) * fb_size;
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
enum { kSingleGlobal = 0, kSingleLocal = 1, kCompound = 2 };

struct Model {
  int type;  // 0 identity, 1 translation, 2 rotzoom, 3 affine
  int is_warp;
  int mat[6];
  int16_t abgd[4];  // alpha, beta, gamma, delta
};

struct Block {
  int kind;
  int mi_row, mi_col;
  int bw_log, bh_log;  // log2 of the luma size in units of 4 samples
  int ref[2];          // 0..6
  int noskip;
  Model local;    // kSingleLocal
  int mv[2][2];   // row, col in 1/8 luma sample, for the references of a compound block without a warp
  int comp_type;  // REF_COMP_*
  int fwd, bck;   // REF_COMP_DISTANCE
  int wedge_index, wedge_sign, mask_type;
  int filter_x, filter_y;
  int luma_w() const { return 4 << bw_log; }
  int luma_h() const { return 4 << bh_log; }
  int has_chroma() const { return bw_log >= 2 && bh_log >= 2; }  // chroma is predicted by these shaders too
};

struct Case {
  std::string name;
  Layout l;
  int bit_depth;
  int content;
  Model gm[7];
  int ref_frame[7];  // which of the two reference frames of the pool a reference is: 1 or 2
  std::vector<Block> blocks;
};

static std::string describe(const Case &c, const Block &b) {
  static const char *const kinds[] = {"GLOBALMV, global warp", "local warp", "GLOBAL_GLOBALMV"};
  static const char *const types[] = {"identity", "translation", "rotzoom", "affine"};
  static const char *const comps[] = {"average", "distance weights", "wedge", "difference weights"};
  char text[400];
  int n = snprintf(text, sizeof(text), "%dx%d block at x %d y %d, %s, ref %d", b.luma_w(), b.luma_h(), b.mi_col * 4,
                   b.mi_row * 4, kinds[b.kind], b.ref[0] + 1);
  if (b.kind == kCompound) {
    for (int r = 0; r < 2; r++) {
      const Model &m = c.gm[b.ref[r]];
      if (r) n += snprintf(text + n, sizeof(text) - n, " + ref %d", b.ref[1] + 1);
      if (m.is_warp)
        n += snprintf(text + n, sizeof(text) - n, " (%s warp)", types[m.type]);
      else
        n += snprintf(text + n, sizeof(text) - n, " (%s, mv %d,%d)", types[m.type], b.mv[r][0], b.mv[r][1]);
    }
    n += snprintf(text + n, sizeof(text) - n, ", %s", comps[b.comp_type]);
  } else if (b.kind == kSingleGlobal) {
    n += snprintf(text + n, sizeof(text) - n, " (%s)", types[c.gm[b.ref[0]].type]);
  }
  snprintf(text + n, sizeof(text) - n, "%s", b.noskip ? ", with residual" : "");
  return text;
}

// A warp model libaom accepts, or identity.
static void random_model(Model *m, int type, int spread) {
  memset(m, 0, sizeof(*m));
  m->mat[2] = m->mat[5] = 1 << 16;
  m->type = type;
  if (type == 0) return;
  if (type == 1) {
    // GM_TRANS_ONLY_DECODE_FACTOR, without high precision vectors: steps of 1/4 sample, up to 64 samples.
    m->mat[0] = rnd_range(-256, 256) * (1 << 14);
    m->mat[1] = rnd_range(-256, 256) * (1 << 14);
    return;
  }
  for (int tries = 0; tries < 64; tries++) {
    // GM_ALPHA_DECODE_FACTOR 2 and GM_TRANS_DECODE_FACTOR 1024, as read_global_motion_params() builds them.
    m->mat[0] = rnd_range(-4096, 4096) * 1024;
    m->mat[1] = rnd_range(-4096, 4096) * 1024;
    m->mat[2] = (1 << 16) + 2 * rnd_range(-spread, spread);
    m->mat[3] = 2 * rnd_range(-spread, spread);
    if (type == 2) {
      m->mat[4] = -m->mat[3];
      m->mat[5] = m->mat[2];
    } else {
      m->mat[4] = 2 * rnd_range(-spread, spread);
      m->mat[5] = (1 << 16) + 2 * rnd_range(-spread, spread);
    }
    if (ref_shear(m->mat, m->abgd)) {
      m->is_warp = 1;
      return;
    }
  }
  memset(m, 0, sizeof(*m));
  m->mat[2] = m->mat[5] = 1 << 16;
}

// A local warp model: from sample pairs through find_projection(), or any affine model libaom accepts.
static int random_local_model(Model *m, const Block &b) {
  memset(m, 0, sizeof(*m));
  m->type = 3;
  const int bw = b.luma_w(), bh = b.luma_h();
  if (rnd(2)) {
    int pts[16], pts_inref[16];
    const int np = rnd_range(1, 8);
    const int mv_row = rnd_range(-400, 400), mv_col = rnd_range(-400, 400);
    const int stretch_x = rnd_range(-40, 40), stretch_y = rnd_range(-40, 40), skew = rnd_range(-30, 30);
    for (int i = 0; i < np; i++) {
      // Sample positions around the block, in 1/8 sample, and where they are in the reference.
      const int x = (b.mi_col * 4 + rnd_range(-bw, 2 * bw)) * 8;
      const int y = (b.mi_row * 4 + rnd_range(-bh, 2 * bh)) * 8;
      pts[2 * i] = x;
      pts[2 * i + 1] = y;
      const int rx = x - b.mi_col * 32, ry = y - b.mi_row * 32;
      pts_inref[2 * i] = x + mv_col + (rx * stretch_x + ry * skew) / 512 + rnd_range(-2, 2);
      pts_inref[2 * i + 1] = y + mv_row + (ry * stretch_y - rx * skew) / 512 + rnd_range(-2, 2);
    }
    if (!ref_local_warp(np, pts, pts_inref, bw, bh, mv_row, mv_col, b.mi_row, b.mi_col, m->mat, m->abgd)) return 0;
    m->is_warp = 1;
    return 1;
  }
  const int spread = 1 << rnd_range(4, 12);
  for (int tries = 0; tries < 64; tries++) {
    m->mat[2] = (1 << 16) + rnd_range(-spread, spread);
    m->mat[3] = rnd_range(-spread, spread);
    m->mat[4] = rnd_range(-spread, spread);
    m->mat[5] = (1 << 16) + rnd_range(-spread, spread);
    // The centre of the block moves by up to 100 samples.
    const int cx = b.mi_col * 4 + bw / 2 - 1, cy = b.mi_row * 4 + bh / 2 - 1;
    const int64_t tx = static_cast<int64_t>(rnd_range(-100 * 65536, 100 * 65536)) -
                       (static_cast<int64_t>(m->mat[2] - (1 << 16)) * cx + static_cast<int64_t>(m->mat[3]) * cy);
    const int64_t ty = static_cast<int64_t>(rnd_range(-100 * 65536, 100 * 65536)) -
                       (static_cast<int64_t>(m->mat[4]) * cx + static_cast<int64_t>(m->mat[5] - (1 << 16)) * cy);
    if (tx < -(1ll << 30) || tx > (1ll << 30) || ty < -(1ll << 30) || ty > (1ll << 30)) continue;
    m->mat[0] = static_cast<int>(tx);
    m->mat[1] = static_cast<int>(ty);
    if (ref_shear(m->mat, m->abgd)) {
      m->is_warp = 1;
      return 1;
    }
  }
  return 0;
}

// (w_log, h_log) of the block sizes that can be warped: both sides at least 8 samples.
static const int kSizes[17][2] = {{1, 1}, {1, 2}, {1, 3}, {2, 1}, {2, 2}, {2, 3}, {2, 4}, {3, 1}, {3, 2},
                                  {3, 3}, {3, 4}, {4, 2}, {4, 3}, {4, 4}, {4, 5}, {5, 4}, {5, 5}};

static void random_compound(const Case &c, Block *b) {
  const int bsize = ref_block_size(b->luma_w(), b->luma_h());
  b->comp_type = rnd(4);
  if (b->comp_type == REF_COMP_WEDGE && !ref_wedge_bits(bsize))
    b->comp_type = rnd(2) ? REF_COMP_DIFFWTD : REF_COMP_AVERAGE;
  b->fwd = b->bck = 8;
  if (b->comp_type == REF_COMP_DISTANCE) ref_distance_weights(rnd(4), rnd(2), &b->fwd, &b->bck);
  b->wedge_index = rnd(16);
  b->wedge_sign = rnd(2);
  b->mask_type = rnd(2);
  b->filter_x = rnd(4);
  b->filter_y = rnd(4);
  for (int r = 0; r < 2; r++) {
    const Model &m = c.gm[b->ref[r]];
    b->mv[r][0] = b->mv[r][1] = 0;
    if (m.is_warp) {
      // The vector of a warped reference is not used by the shader; the decoder stores the global vector.
      b->mv[r][0] = rnd_range(-64, 64);
      b->mv[r][1] = rnd_range(-64, 64);
    } else if (m.type == 1) {
      ref_global_mv(m.mat, m.type, 0, b->luma_w(), b->luma_h(), b->mi_row, b->mi_col, &b->mv[r][0], &b->mv[r][1]);
    } else if (rnd(3) == 0) {
      // Not a vector GLOBAL_GLOBALMV can have with an identity model, but the shader takes any: more
      // positions and filter phases at the frame edges.
      const int reach = rnd(2) ? 64 : 1280;
      b->mv[r][0] = rnd_range(-reach, reach);
      b->mv[r][1] = rnd_range(-reach, reach);
    }
  }
}

static void add_blocks(Case *c, int attempts) {
  const Layout &l = c->l;
  const int grid_w = l.y_stride, grid_h = static_cast<int>(l.y_size / l.y_stride) >> 2;
  std::vector<uint8_t> used(static_cast<size_t>(grid_w) * grid_h, 0);
  for (const Block &b : c->blocks)
    for (int y = 0; y < (1 << b.bh_log); y++)
      for (int x = 0; x < (1 << b.bw_log); x++) used[static_cast<size_t>(b.mi_row + y) * grid_w + b.mi_col + x] = 1;
  int warp_refs[7], n_warp = 0;
  for (int r = 0; r < 7; r++)
    if (c->gm[r].is_warp) warp_refs[n_warp++] = r;
  for (int i = 0; i < attempts; i++) {
    Block b = {};
    const int *size = kSizes[rnd(17)];
    b.bw_log = size[0];
    b.bh_log = size[1];
    const int cols = (l.mi_cols + (1 << b.bw_log) - 1) >> b.bw_log;
    const int rows = (l.mi_rows + (1 << b.bh_log) - 1) >> b.bh_log;
    const int at_x = rnd(4), at_y = rnd(4);  // 0 first, 1 last, else anywhere
    b.mi_col = (at_x == 0 ? 0 : at_x == 1 ? cols - 1 : rnd(cols)) << b.bw_log;
    b.mi_row = (at_y == 0 ? 0 : at_y == 1 ? rows - 1 : rnd(rows)) << b.bh_log;
    bool free = true;
    for (int y = 0; y < (1 << b.bh_log) && free; y++)
      for (int x = 0; x < (1 << b.bw_log) && free; x++)
        free = !used[static_cast<size_t>(b.mi_row + y) * grid_w + b.mi_col + x];
    if (!free) continue;
    b.noskip = rnd(2);
    const int pick = rnd(10);
    if (pick < 4 && n_warp) {
      // GLOBAL_GLOBALMV: two references, at least one of them with a warp model.
      b.kind = kCompound;
      b.ref[0] = rnd(7);
      b.ref[1] = rnd(7);
      if (b.ref[0] == b.ref[1]) continue;
      if (!c->gm[b.ref[0]].is_warp && !c->gm[b.ref[1]].is_warp) b.ref[rnd(2)] = warp_refs[rnd(n_warp)];
      if (b.ref[0] == b.ref[1]) continue;
      random_compound(*c, &b);
    } else if (pick < 7 && n_warp) {
      b.kind = kSingleGlobal;
      b.ref[0] = warp_refs[rnd(n_warp)];
    } else {
      b.kind = kSingleLocal;
      b.ref[0] = rnd(7);
      if (!random_local_model(&b.local, b)) continue;
    }
    for (int y = 0; y < (1 << b.bh_log); y++)
      for (int x = 0; x < (1 << b.bw_log); x++) used[static_cast<size_t>(b.mi_row + y) * grid_w + b.mi_col + x] = 1;
    c->blocks.push_back(b);
  }
}

// ---------------------------------------------------------------------------
// One case: the pool, the block records, the shaders, libaom, the comparison.
// ---------------------------------------------------------------------------
static std::vector<uint8_t> g_warp_filter;  // inter_warp_filter: the filter taps as 32-bit integers
static std::vector<uint8_t> g_wedge_lut;    // inter_mask_lut
static int g_wedge_offsets[22][2];
static int g_kernels[8 * 16][8];

struct Stats {
  long cases = 0, blocks = 0, single_global = 0, single_local = 0, compound = 0, compound_mv_side = 0;
  long mismatches[2] = {0, 0};  // cases that differ, by double model
  long shown = 0;
  long double_ops = 0;
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
  const int w = c.l.plane_w(p), h = c.l.plane_h(p);
  switch (c.content) {
    case 0:
      return rnd(max + 1);
    case 1:
      return std::min(
          max, std::max(0, (x * 5 + y * 3 + frame * 37 + p * 91) * max / (5 * w + 3 * h + 200) + rnd_range(-6, 6)));
    case 2:
      return rnd(40) ? (max * (3 + frame + p)) >> 3 : rnd(max + 1);
    case 3:
      return ((x >> 3) + (y >> 3) + frame) & 1 ? max : 0;
    default: {
      // A picture between black bars: a wrong position shows as a dark block.
      const int black = max >> 4;
      if (y < h / 8 || y >= h - h / 8) return p ? (max + 1) >> 1 : black;
      return std::min(max, black + ((x * 7 + y * 11 + frame * 53) % (max - black)) / 2 + (max >> 2) + rnd_range(0, 3));
    }
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
  std::vector<uint32_t> words;  // the records of one block list for one block size
  int count = 0;
};

static bool run_case(const Case &c, Stats *stats, int verbose) {
  const Layout &l = c.l;
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
    for (int p = 0; p < 3; p++) {
      const int w = l.plane_w(p), h = l.plane_h(p);
      const int pw = w + 2 * kRefBorder, ph = h + 2 * kRefBorder;
      std::vector<uint16_t> picture(static_cast<size_t>(w) * h);
      for (int y = 0; y < h; y++)
        for (int x = 0; x < w; x++) {
          const int v = picture_sample(c, frame, p, x, y, max);
          picture[static_cast<size_t>(y) * w + x] = static_cast<uint16_t>(v);
          put_sample(&pool[l.offset(frame, p) + static_cast<size_t>(y) * l.stride(p) + static_cast<size_t>(x) * bpp],
                     hbd, v);
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

  // Block records, as gen_pred_blocks.hlsl writes them, sorted into lists by pass and block size.
  std::map<std::pair<int, int>, List> warp_lists, comp_lists, comp_diff_uv_lists;
  for (const Block &b : c.blocks) {
    const uint32_t noskip = b.noskip ? 0x2000u : 0;
    if (b.kind != kCompound) {
      const Model &m = b.kind == kSingleLocal ? b.local : c.gm[b.ref[0]];
      for (int p = 0; p < (b.has_chroma() ? 3 : 1); p++) {
        const int ss = p > 0;
        List &list = warp_lists[std::make_pair(b.bw_log - ss, b.bh_log - ss)];
        list.words.push_back(static_cast<uint32_t>(b.mi_col >> ss) | (static_cast<uint32_t>(b.mi_row >> ss) << 16));
        list.words.push_back(static_cast<uint32_t>(p) | (static_cast<uint32_t>(b.ref[0]) << 2) | noskip);
        for (int i = 0; i < 6; i++) list.words.push_back(static_cast<uint32_t>(m.mat[i]));
        list.words.push_back(static_cast<uint32_t>(static_cast<int>(m.abgd[0])));  // alpha
        list.words.push_back(static_cast<uint32_t>(static_cast<int>(m.abgd[1])));  // beta
        list.words.push_back(static_cast<uint32_t>(static_cast<int>(m.abgd[3])));  // delta
        list.words.push_back(static_cast<uint32_t>(static_cast<int>(m.abgd[2])));  // gamma
        list.count++;
      }
      continue;
    }
    const int bsize = ref_block_size(b.luma_w(), b.luma_h());
    const int wedge_idx = b.wedge_sign + b.wedge_index * 2;
    for (int p = 0; p < (b.has_chroma() ? 3 : 1); p++) {
      const int ss = p > 0;
      const int w_log = b.bw_log - ss, h_log = b.bh_log - ss;
      uint32_t wtd = 0x88;
      if (b.comp_type == REF_COMP_DISTANCE) wtd = static_cast<uint32_t>(b.fwd | (b.bck << 4));
      if (b.comp_type == REF_COMP_WEDGE)
        wtd = static_cast<uint32_t>(g_wedge_offsets[bsize][ss] + (wedge_idx << std::max(0, w_log + h_log - 2)));
      if (b.comp_type == REF_COMP_DIFFWTD) wtd = static_cast<uint32_t>(b.mask_type);
      const uint32_t gpu_comp_type = ((ss ? 0x3100u : 0x2100u) >> (b.comp_type * 4)) & 15;
      List &list =
          (ss && b.comp_type == REF_COMP_DIFFWTD ? comp_diff_uv_lists : comp_lists)[std::make_pair(w_log, h_log)];
      list.words.push_back(static_cast<uint32_t>(b.mi_col >> ss) | (static_cast<uint32_t>(b.mi_row >> ss) << 16));
      list.words.push_back(static_cast<uint32_t>(p) | (static_cast<uint32_t>(b.ref[0]) << 2) |
                           (filter_type(b.filter_x, w_log) << 5) | (filter_type(b.filter_y, h_log) << 9) |
                           (static_cast<uint32_t>(b.ref[1]) << 14) | (wtd << 17) | noskip | (gpu_comp_type << 30));
      // Luma vectors are stored doubled (1/16 sample), chroma vectors as they are.
      list.words.push_back(ss ? mv_word(b.mv[0]) : (mv_word(b.mv[0]) << 1) & 0xfffeffffu);
      list.words.push_back(ss ? mv_word(b.mv[1]) : (mv_word(b.mv[1]) << 1) & 0xfffeffffu);
      list.count++;
    }
  }
  std::vector<uint32_t> warp_words, pred_words;
  struct Pass {
    int kind;  // 0 inter_warp, 1 compound
    uint32_t srt[4];
  };
  std::vector<Pass> passes;
  for (auto &entry : warp_lists) {
    const int w = entry.first.first, h = entry.first.second;
    passes.push_back(
        {0,
         {static_cast<uint32_t>((1 << (w + h)) * entry.second.count), static_cast<uint32_t>(warp_words.size() / 12),
          static_cast<uint32_t>(w), static_cast<uint32_t>(h)}});
    warp_words.insert(warp_words.end(), entry.second.words.begin(), entry.second.words.end());
  }
  for (auto *lists : {&comp_lists, &comp_diff_uv_lists}) {
    for (auto &entry : *lists) {
      const int w = entry.first.first, h = entry.first.second;
      passes.push_back(
          {1,
           {static_cast<uint32_t>((4 << (w + h)) * entry.second.count), static_cast<uint32_t>(pred_words.size() / 4),
            static_cast<uint32_t>(w), static_cast<uint32_t>(h)}});
      pred_words.insert(pred_words.end(), entry.second.words.begin(), entry.second.words.end());
    }
  }
  // The lists are followed by garbage: a thread that looks past its list must not get a valid block.
  for (int i = 0; i < 64; i++) {
    warp_words.push_back(static_cast<uint32_t>(rnd64()));
    pred_words.push_back(static_cast<uint32_t>(rnd64()));
  }

  ShaderBuffers buffers = {};
  buffers.pred_blocks = reinterpret_cast<const uint8_t *>(pred_words.data());
  buffers.pred_blocks_size = pred_words.size() * 4;
  buffers.warp_blocks = reinterpret_cast<const uint8_t *>(warp_words.data());
  buffers.warp_blocks_size = warp_words.size() * 4;
  buffers.residuals = residuals.data();
  buffers.residuals_size = residuals.size();
  buffers.comp_mask = g_wedge_lut.data();
  buffers.comp_mask_size = g_wedge_lut.size();
  buffers.warp_filter = g_warp_filter.data();
  buffers.warp_filter_size = g_warp_filter.size();
  buffers.frame = pool.data();
  buffers.frame_size = pool.size();

  // The border of the reference frames, extended by the decoder's own shader when a frame is done.
  const ShaderApi *border_api = hbd ? &inter_ext_borders_hbd_api : &inter_ext_borders_api;
  for (int frame = 1; frame <= 2; frame++) {
    BorderConsts bc = {};
    for (int p = 0; p < 3; p++) {
      bc.planes[p][0] = l.stride(p);
      bc.planes[p][1] = l.offset(frame, p);
    }
    bc.dims[0][0] = l.plane_w(0);
    bc.dims[0][1] = l.plane_h(0);
    bc.dims[1][0] = l.plane_w(1);
    bc.dims[1][1] = l.plane_h(1);
    border_api->bind(&buffers);
    border_api->set_common(&bc);
    dispatch(border_api, static_cast<uint32_t>(l.plane_h(0) + 2 * l.plane_h(1)));
  }

  // PSSLInterData, as av1_prediction_run_all() fills it. What it leaves unset is garbage here.
  static InterConsts consts;
  fill_garbage(reinterpret_cast<uint8_t *>(&consts), sizeof(consts));
  for (int p = 0; p < 3; p++) {
    consts.planes[p][0] = l.stride(p);
    consts.planes[p][1] = l.offset(0, p);
    consts.planes[p][2] = l.res_stride[p];
    consts.planes[p][3] = l.res_offset[p];
  }
  for (int r = 0; r < 7; r++)
    for (int p = 0; p < 3; p++) {
      int *rp = consts.refplanes[r * 3 + p];
      rp[0] = l.stride(p);
      rp[1] = l.offset(c.ref_frame[r], p);
      rp[2] = l.plane_w(p) - 1;
      rp[3] = l.plane_h(p) - 1;
    }
  consts.dims[0][0] = l.plane_w(0) - 1;
  consts.dims[0][1] = l.plane_h(0) - 1;
  consts.dims[1][0] = l.plane_w(1) - 1;
  consts.dims[1][1] = l.plane_h(1) - 1;
  memcpy(consts.kernels, g_kernels, sizeof(consts.kernels));
  for (int r = 1; r < 8; r++) {
    // td->scale_factors of a frame whose references have its size: REF_NO_SCALE and a step of one sample.
    consts.scale[r][0] = consts.scale[r][2] = 1 << 14;
    consts.scale[r][1] = consts.scale[r][3] = 1 << 10;
  }
  for (int r = 0; r < 7; r++) {
    const Model &m = c.gm[r];
    consts.warp[r][0] = m.is_warp;
    if (!m.is_warp) continue;
    for (int i = 0; i < 6; i++) consts.warp[r][2 + i] = m.mat[i];
    consts.warp[r][8] = m.abgd[0];
    consts.warp[r][9] = m.abgd[1];
    consts.warp[r][10] = m.abgd[3];
    consts.warp[r][11] = m.abgd[2];
  }

  // What libaom makes of the same blocks.
  std::vector<uint8_t> expected(pool);
  std::vector<uint16_t> block16(128 * 128);
  std::vector<uint8_t> block8(128 * 128), seg_mask(128 * 128);
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
      rb.refs = b.kind == kCompound ? 2 : 1;
      for (int r = 0; r < rb.refs; r++) {
        RefSide &s = rb.side[r];
        const int frame = c.ref_frame[b.ref[r]];
        const int pw = l.plane_w(p) + 2 * kRefBorder;
        const size_t origin = static_cast<size_t>(kRefBorder) * pw + kRefBorder;
        s.origin = hbd ? static_cast<const void *>(&padded16[frame][p][origin])
                       : static_cast<const void *>(&padded8[frame][p][origin]);
        s.stride = pw;
        s.width = l.plane_w(p);
        s.height = l.plane_h(p);
        const Model &m = b.kind == kSingleLocal ? b.local : c.gm[b.ref[r]];
        s.kind = b.kind == kSingleLocal ? REF_SIDE_LOCAL_WARP : m.is_warp ? REF_SIDE_GLOBAL_WARP : REF_SIDE_MV;
        memcpy(s.mat, m.mat, sizeof(s.mat));
        if (s.kind == REF_SIDE_MV) {
          s.mv_row = b.mv[r][0];
          s.mv_col = b.mv[r][1];
        }
      }
      rb.filter_x = b.filter_x;
      rb.filter_y = b.filter_y;
      rb.comp_type = b.comp_type;
      rb.fwd_offset = b.fwd;
      rb.bck_offset = b.bck;
      rb.wedge_index = b.wedge_index;
      rb.wedge_sign = b.wedge_sign;
      rb.mask_type = b.mask_type;
      rb.seg_mask = seg_mask.data();
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
              &expected[l.offset(0, p) + static_cast<size_t>(py + y) * l.stride(p) + static_cast<size_t>(px + x) * bpp],
              hbd, v);
        }
      if (p == 0 && b.kind == kCompound && b.comp_type == REF_COMP_DIFFWTD) {
        // The luma pass of a difference weighted block leaves the mask for chroma in both chroma planes,
        // where the chroma pass reads it before it writes its samples: four values, each the rounded mean
        // of 2x2 luma values, as one word at the first of the four chroma samples. Chroma that is too
        // small for a warp is not predicted by these shaders, so there the words stay.
        for (int y = 0; y < h; y += 2)
          for (int x = 0; x < w; x += 8) {
            uint32_t word = 0;
            for (int i = 0; i < 4; i++) {
              const uint8_t *m = &seg_mask[static_cast<size_t>(y) * w + x + 2 * i];
              word |= static_cast<uint32_t>((m[0] + m[1] + m[w] + m[w + 1] + 2) >> 2) << (8 * i);
            }
            for (int cp = 1; cp < 3; cp++)
              memcpy(&expected[l.offset(0, cp) + static_cast<size_t>((py + y) >> 1) * l.stride(cp) +
                               static_cast<size_t>((px + x) >> 1) * bpp],
                     &word, 4);
          }
      }
    }
  }

  // The shaders, in the order of av1_prediction_run_all(): warp, compound warp, then the chroma of
  // difference weighted compound blocks, which reads the mask the luma pass has left in the chroma planes.
  const ShaderApi *apis[2] = {hbd ? &inter_warp_hbd_api : &inter_warp_api,
                              hbd ? &inter_warp_comp_hbd_api : &inter_warp_comp_api};
  bool ok = true;
  for (int model = 1; model >= 0; model--) {
    std::vector<uint8_t> result(pool);
    buffers.frame = result.data();
    g_double_model = model;
    const long ops_before = g_double_ops, oob_before = g_out_of_bounds;
    for (const Pass &pass : passes) {
      const ShaderApi *api = apis[pass.kind];
      api->bind(&buffers);
      api->set_common(&consts);
      api->set_srt(pass.srt);
      dispatch(api, pass.srt[0]);
    }
    g_double_model = 1;
    stats->double_ops += g_double_ops - ops_before;
    const bool same = memcmp(result.data(), expected.data(), pool_size) == 0;
    if (same && g_out_of_bounds == oob_before) continue;
    ok = false;
    stats->mismatches[model]++;
    if (stats->shown >= 12 && !verbose) continue;
    stats->shown++;
    printf("MISMATCH %s, %dx%d %d-bit, %s double precision", c.name.c_str(), l.w, l.h, c.bit_depth,
           model ? "with" : "without");
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
        const long rel = static_cast<long>(at) - (l.offset(frame, p) - 16 * bpp);
        const long rows = static_cast<long>((p ? l.uv_size : l.y_size) / (p ? l.uv_stride : l.y_stride));
        if (rel < 0 || rel >= rows * l.stride(p)) continue;
        located = true;
        const int y = static_cast<int>(rel / l.stride(p)), x = static_cast<int>(rel % l.stride(p)) / bpp - 16;
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
            printf("  in the %s\n", describe(c, b).c_str());
        }
      }
    if (!located) printf("  %ld bytes differ; first at pool offset %zu, outside the frames\n", wrong, at);
  }
  stats->cases++;
  for (const Block &b : c.blocks) {
    stats->blocks++;
    stats->single_global += b.kind == kSingleGlobal;
    stats->single_local += b.kind == kSingleLocal;
    stats->compound += b.kind == kCompound;
    stats->compound_mv_side += b.kind == kCompound && (!c.gm[b.ref[0]].is_warp || !c.gm[b.ref[1]].is_warp);
  }
  return ok;
}

// ---------------------------------------------------------------------------
// scale_value() of inter_common.h against scaled_x() of libaom's scale.c.
// ---------------------------------------------------------------------------
static long check_scale_value(int model, long *checked) {
  static const int scales[] = {1 << 14, (1 << 14) + 1, (1 << 14) - 1, 1 << 13, 1 << 15, 1 << 10, 1025, 20000,
                               12345,   9362,          24576,         16639,   16129,   32767,   8191, 1 << 12};
  static const int values[] = {0,   1,   -1,    15,   16,    17,    -16,     -17,       255,
                               256, 257, 29696, 9984, 65535, 65536, 1 << 20, -(1 << 20)};
  long wrong = 0;
  g_double_model = model;
  for (int i = 0; i < 400000; i++) {
    const int scale = i < 16 * 17 ? scales[i % 16] : i % 3 ? scales[rnd(16)] : rnd_range(1 << 10, 1 << 15);
    // A position in 1/16 sample: up to 65536 samples, and a vector on top of it.
    const int val = i < 16 * 17 ? values[i / 16]
                    : rnd(8)    ? rnd_range(-(1 << 15), (1 << 20) + (1 << 15))
                                : rnd_range(-600, 600);
    const int got = inter_warp_comp_hbd_api.scale_value(val, scale), want = ref_scale_value(val, scale);
    if (got != inter_warp_comp_api.scale_value(val, scale) || got != inter_warp_api.scale_value(val, scale)) abort();
    (*checked)++;
    if (got == want) continue;
    if (wrong++ < 3)
      printf("MISMATCH scale_value(%d, %d) %s double precision: shader %d, libaom %d\n", val, scale,
             model ? "with" : "without", got, want);
  }
  g_double_model = 1;
  return wrong;
}

// ---------------------------------------------------------------------------
// The cases.
// ---------------------------------------------------------------------------
static void random_case(Case *c, int w, int h, int bit_depth, int index) {
  c->l = make_layout(w, h, bit_depth > 8);
  c->bit_depth = bit_depth;
  c->content = index % 5;
  c->name = "case " + std::to_string(index);
  const int spread = 1 << rnd_range(3, 12);
  for (int r = 0; r < 7; r++) {
    static const int types[8] = {0, 0, 1, 2, 2, 2, 3, 3};
    random_model(&c->gm[r], types[rnd(8)], spread);
    c->ref_frame[r] = 1 + rnd(2);
  }
  add_blocks(c, 6 + rnd(30));
}

// The block of the console failure this check was written for: a 1080p 10-bit frame, a 32x16
// GLOBAL_GLOBALMV wedge block 32 samples from the right frame edge, LAST without global motion and ALTREF
// with a rotzoom model.
static void console_case(Case *c) {
  c->l = make_layout(1920, 1080, 1);
  c->bit_depth = 10;
  c->content = 4;
  c->name = "console case";
  for (int r = 0; r < 7; r++) {
    random_model(&c->gm[r], 0, 0);
    c->ref_frame[r] = 1 + (r == 6);
  }
  Model &m = c->gm[6];
  static const int mat[6] = {-267264, 381952, 65484, 98, -98, 65484};
  memcpy(m.mat, mat, sizeof(mat));
  m.type = 2;
  m.is_warp = ref_shear(m.mat, m.abgd);
  Block b = {};
  b.kind = kCompound;
  b.mi_col = 1856 / 4;
  b.mi_row = 624 / 4;
  b.bw_log = 3;
  b.bh_log = 2;
  b.ref[0] = 0;
  b.ref[1] = 6;
  b.noskip = 1;
  b.comp_type = REF_COMP_WEDGE;
  b.fwd = b.bck = 8;
  b.wedge_index = 5;
  ref_global_mv(m.mat, m.type, 0, 32, 16, b.mi_row, b.mi_col, &b.mv[1][0], &b.mv[1][1]);
  c->blocks.push_back(b);
}

int main(int argc, char **argv) {
  int cases_per_size = 24, verbose = 0;
  for (int i = 1; i < argc; i++) {
    if (!strcmp(argv[i], "--cases") && i + 1 < argc) {
      cases_per_size = atoi(argv[++i]);
    } else if (!strcmp(argv[i], "--seed") && i + 1 < argc) {
      g_rng ^= strtoull(argv[++i], nullptr, 0) * 0x9e3779b97f4a7c15ull;
    } else if (!strcmp(argv[i], "-v")) {
      verbose = 1;
    } else {
      fprintf(stderr, "usage: warp_check [--cases N] [--seed S] [-v]\n");
      return 2;
    }
  }

  ref_init();
  const int16_t *filter = ref_warp_filter();
  g_warp_filter.resize(193 * 8 * 4);
  for (int i = 0; i < 193 * 8; i++) {
    const int tap = filter[i];  // as av1_compute.cpp uploads inter_warp_filter
    memcpy(&g_warp_filter[static_cast<size_t>(i) * 4], &tap, 4);
  }
  ref_kernels(g_kernels);
  g_wedge_lut.resize(1 << 18);
  g_wedge_lut.resize(ref_wedge_lut(g_wedge_lut.data(), g_wedge_lut.size(), g_wedge_offsets));

  bool ok = true;

  long scale_checked = 0;
  const long scale_wrong[2] = {check_scale_value(0, &scale_checked), check_scale_value(1, &scale_checked)};
  printf("warp_check: scale_value() against libaom's scaled_x(), %ld values: %s with double precision, %s without\n",
         scale_checked / 2, scale_wrong[1] ? "DIFFERENT" : "identical", scale_wrong[0] ? "DIFFERENT" : "identical");
  ok &= !scale_wrong[0] && !scale_wrong[1];

  Stats stats;
  {
    Case c;
    console_case(&c);
    const bool same = run_case(c, &stats, verbose);
    printf(
        "warp_check: console case (1920x1080 10-bit, 32x16 GLOBAL_GLOBALMV wedge block at x 1856 y 624, LAST plain "
        "and ALTREF rotzoom): %s\n",
        same ? "identical" : "DIFFERENT");
    ok &= same;
  }

  // Frame sizes: multiples of 64 and of 8, sizes that are neither, odd sizes, frames smaller than a block.
  static const int sizes[][3] = {{64, 64, 4},   {72, 40, 4},   {131, 67, 4},    {200, 120, 4},
                                 {256, 136, 4}, {176, 144, 4}, {16, 16, 2},     {33, 17, 2},
                                 {8, 8, 1},     {853, 480, 2}, {1918, 1078, 0}, {1920, 1080, 0}};
  int index = 0;
  for (int bit_depth = 8; bit_depth <= 10; bit_depth += 2) {
    for (const auto &size : sizes) {
      // The large frames cost the most time per case: a few of them are enough.
      const int count = size[2] ? std::max(1, cases_per_size * size[2] / 4) : std::max(1, cases_per_size / 12);
      for (int i = 0; i < count; i++) {
        Case c;
        random_case(&c, size[0], size[1], bit_depth, index++);
        ok &= run_case(c, &stats, verbose);
      }
    }
  }
  printf(
      "warp_check: %ld frames, 8-bit and 10-bit, %ld blocks: %ld GLOBALMV with a global warp, %ld with a local warp, "
      "%ld GLOBAL_GLOBALMV (%ld of them with one reference not warped)\n",
      stats.cases, stats.blocks, stats.single_global, stats.single_local, stats.compound, stats.compound_mv_side);
  printf("warp_check: frames that differ from libaom: %ld with double precision, %ld without\n", stats.mismatches[1],
         stats.mismatches[0]);
  printf("warp_check: double precision operations executed by the shaders: %ld%s\n", stats.double_ops,
         stats.double_ops ? " (the console does not execute them)" : "");
  ok &= stats.double_ops == 0;
  if (g_out_of_bounds) printf("warp_check: %ld accesses outside the buffers\n", g_out_of_bounds);
  ok &= g_out_of_bounds == 0;
  printf("warp_check: %ld group syncs; races in group memory: %ld, group syncs not reached by all threads: %ld, "
         "plain group memory barriers: %ld\n",
         gs_state.group_syncs, gs_state.races, gs_state.sync_errors, gs_state.plain_barriers);
  gs_print_races("  ");
  ok &= gs_state.races == 0 && gs_state.sync_errors == 0 && gs_state.plain_barriers == 0 && gs_state.out_of_bounds == 0;
  printf(ok ? "warp_check: the warp shaders and libaom write identical frames\n" : "warp_check: FAILED\n");
  return ok ? 0 : 1;
}
