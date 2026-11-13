/*
 * Summary maths for the -gputime timing run of the test player.
 *
 * Header only and free of platform dependencies (needs just dx/av1_perf.h),
 * so it can be compiled and checked on any host.
 */

#pragma once

#include <stdarg.h>
#include <stdint.h>
#include <stdio.h>
#include <algorithm>
#include <map>
#include <string>
#include <vector>

#include "dx/av1_perf.h"

namespace perfstats {

static const char* const kGpuStageNames[] = {
    "gen_blocks",   "residual_clear", "idct_sort",  "idct",        "inter",      "recon",
    "intra",        "lf_gen",         "lf_vert",    "lf_horz",     "cdef",       "superres",
    "loop_rest",    "ext_borders",    "film_grain", "copy_plane",  "output_copy"};
static_assert(sizeof(kGpuStageNames) / sizeof(kGpuStageNames[0]) == AV1_PERF_GPU_STAGE_COUNT,
              "GPU stage names out of step with av1_perf.h");

static const char* const kCpuNames[] = {
    "wait_slot",     "wait_sec_slot", "wait_fb",        "wait_output",    "wait_image",  "wait_item",
    "setup",         "memset",        "memset_tile",    "entropy_wall",   "entropy_sum", "record",
    "rec_gen_blocks", "rec_idct",     "rec_pred",       "rec_loopfilter", "rec_cdef_lr", "rec_borders",
    "rec_copy",      "submit",        "frame",          "fence_wait",     "gpu_latency", "notify",
    "async_clear",   "sink_lists",    "sink_calls"};
static_assert(sizeof(kCpuNames) / sizeof(kCpuNames[0]) == AV1_PERF_CPU_COUNT,
              "CPU timer names out of step with av1_perf.h");

// The sections of the inter breakdown (-gputimedetail), in the order of the command list.
// -interfast: single.<class> and comp_avg.<class of reference 0><class of reference 1>, the classes by the
// fraction of the motion vector: copy / c none, h in x only, v in y only, 2d / 2 both.
static const char* const kInterSectionNames[] = {
    "warp",        "single",      "single.copy",  "single.h",          "single.v",    "single.2d",   "comp_avg",
    "comp_avg.cc", "comp_avg.hc", "comp_avg.vc",  "comp_avg.2c",       "comp_avg.ch", "comp_avg.hh", "comp_avg.vh",
    "comp_avg.2h", "comp_avg.cv", "comp_avg.hv",  "comp_avg.vv",       "comp_avg.2v", "comp_avg.c2", "comp_avg.h2",
    "comp_avg.v2", "comp_avg.22", "comp_diff_y",  "comp_masked",       "comp_warp",   "chroma_2x2",  "sync0",
    "comp_diff_uv", "comp_warp_diff_uv", "obmc_above", "sync1",        "obmc_left",   "sync2",       "recon"};
static_assert(sizeof(kInterSectionNames) / sizeof(kInterSectionNames[0]) == AV1_PERF_INTER_SECTION_COUNT,
              "inter section names out of step with av1_perf.h");
// The summary adds these up field by field.
static_assert(sizeof(av1_perf_inter_work) == 7 * sizeof(int64_t), "av1_perf_inter_work is seven 64-bit counters");
static_assert(sizeof(av1_perf_inter_blocks) == 20 * sizeof(int64_t), "av1_perf_inter_blocks is twenty 64-bit counters");
// The pass a section of the inter stage belongs to (0..2), -1 for recon, which is a stage of its own.
static inline int InterPassOf(int section) {
  return section <= AV1_PERF_INTER_SYNC0 ? 0 : section <= AV1_PERF_INTER_SYNC1 ? 1 : section <= AV1_PERF_INTER_SYNC2 ? 2 : -1;
}
static inline bool InterIsSync(int section) {
  return section == AV1_PERF_INTER_SYNC0 || section == AV1_PERF_INTER_SYNC1 || section == AV1_PERF_INTER_SYNC2;
}

// The waits that stall the decode thread (pool starvation), as opposed to work.
static const int kBlockedIds[] = {AV1_PERF_CPU_WAIT_SLOT,   AV1_PERF_CPU_WAIT_SEC_SLOT, AV1_PERF_CPU_WAIT_FB,
                                  AV1_PERF_CPU_WAIT_OUTPUT, AV1_PERF_CPU_WAIT_IMAGE,    AV1_PERF_CPU_WAIT_ITEM};

enum { kWarmupFrames = 10, kMaxFrames = 20000 };
static const double kTargetMs = 1000.0 / 24.0;

class Series {
 public:
  void Add(double v) { v_.push_back(v); }
  size_t Count() const { return v_.size(); }
  double Sum() const {
    double s = 0.0;
    for (size_t i = 0; i < v_.size(); ++i) s += v_[i];
    return s;
  }
  double Mean() const { return v_.empty() ? 0.0 : Sum() / (double)v_.size(); }
  double Max() const { return v_.empty() ? 0.0 : *std::max_element(v_.begin(), v_.end()); }
  // Nearest-rank percentile, p in [0, 100]. 0 for an empty series.
  double Percentile(double p) const {
    if (v_.empty()) return 0.0;
    std::vector<double> s(v_);
    std::sort(s.begin(), s.end());
    double rank = p / 100.0 * (double)s.size();
    size_t idx = (size_t)rank;
    if ((double)idx < rank) ++idx;  // ceil
    if (idx < 1) idx = 1;
    if (idx > s.size()) idx = s.size();
    return s[idx - 1];
  }

 private:
  std::vector<double> v_;
};

// The inter breakdown of one frame as the statistics keep it: the sections without their fixed list arrays,
// and the dispatches that happened.
struct InterFrame {
  struct Section {
    double ms;
    int timed;
    int dispatches;
    int groups;
    int64_t threads;
    int64_t records;
    int64_t pixels;
    int64_t loaded_bytes;
    av1_perf_inter_work work;
  };
  struct List {
    int section;
    av1_perf_inter_list list;
  };
  int mode;
  int has_inter;
  int scaled;
  int bytes_per_sample;
  int barriers_added;
  int marks;
  Section sections[AV1_PERF_INTER_SECTION_COUNT];
  av1_perf_inter_blocks blocks;
  std::vector<List> lists;

  explicit InterFrame(const av1_perf_inter_detail& d)
      : mode(d.mode), has_inter(d.has_inter), scaled(d.scaled), bytes_per_sample(d.bytes_per_sample),
        barriers_added(d.barriers_added), marks(d.marks), blocks(d.blocks) {
    for (int i = 0; i < AV1_PERF_INTER_SECTION_COUNT; ++i) {
      const av1_perf_inter_section& src = d.sections[i];
      Section& dst = sections[i];
      dst.ms = src.ms;
      dst.timed = src.timed;
      dst.dispatches = src.dispatches;
      dst.groups = src.groups;
      dst.threads = src.threads;
      dst.records = src.records;
      dst.pixels = src.pixels;
      dst.loaded_bytes = src.loaded_bytes;
      dst.work = src.work;
      for (int l = 0; l < AV1_PERF_INTER_LISTS; ++l) {
        if (!src.lists[l].records) continue;
        List item = {i, src.lists[l]};
        lists.push_back(item);
      }
    }
  }
  double PassMs(int pass) const {
    double ms = 0.0;
    for (int i = 0; i < AV1_PERF_INTER_SECTION_COUNT; ++i)
      if (InterPassOf(i) == pass) ms += sections[i].ms;
    return ms;
  }
  double InterMs() const { return PassMs(0) + PassMs(1) + PassMs(2); }

  // The two sides of the counters against each other: what the recording dispatched (block records and their
  // samples) and what the tile threads counted when they pushed the blocks. A luma block is one record, a
  // chroma block two; every inter block has its luma in exactly one section that is not an overlapped
  // prediction. Empty when they agree, else what differs first.
  std::string Mismatch() const {
    char b[200];
    int64_t luma = 0;
    for (int i = 0; i < AV1_PERF_INTER_RECON; ++i) {
      const Section& s = sections[i];
      const int64_t blocks = s.work.blocks_y + 2 * s.work.blocks_uv;
      const int64_t pixels = s.work.pixels_y + s.work.pixels_uv;
      if (s.records != blocks || s.pixels != pixels) {
        snprintf(b, sizeof(b), "section %d: %lld records and %lld samples dispatched, %lld blocks (Y + 2 UV) and %lld "
                               "samples pushed",
                 i, (long long)s.records, (long long)s.pixels, (long long)blocks, (long long)pixels);
        return b;
      }
      if (i != AV1_PERF_INTER_OBMC_ABOVE && i != AV1_PERF_INTER_OBMC_LEFT) luma += s.work.pixels_y;
    }
    if (luma != blocks.area) {
      snprintf(b, sizeof(b), "%lld luma samples in the sections, %lld in the inter blocks", (long long)luma,
               (long long)blocks.area);
      return b;
    }
    return std::string();
  }
};

struct OutputWaits {
  int count;
  double total_ms;
  double max_ms;
};

class PerfStats {
 public:
  void Reset() {
    frames_.clear();
    outputs_.clear();
    decode_calls_.clear();
    inter_.clear();
    inter_index_.clear();
    dropped_ = 0;
  }
  // Reset, and the memory of the records goes back to the heap (end of a task).
  void Release() {
    std::vector<av1_perf_frame>().swap(frames_);
    std::vector<double>().swap(outputs_);
    std::vector<double>().swap(decode_calls_);
    std::vector<InterFrame>().swap(inter_);
    std::vector<int>().swap(inter_index_);
    dropped_ = 0;
  }
  // One record per command list the decoder submitted, in completion order: a decoded frame, or the output
  // copy of a show_existing_frame (is_sef), which decodes nothing.
  void AddFrame(const av1_perf_frame& f) {
    if (frames_.size() >= kMaxFrames) {
      ++dropped_;
      return;
    }
    frames_.push_back(f);
    // The inter breakdown (-gputimedetail) is valid during the callback only: keep what the summary needs.
    frames_.back().inter = NULL;
    inter_index_.push_back(f.inter ? (int)inter_.size() : -1);
    if (f.inter) inter_.push_back(InterFrame(*f.inter));
  }
  // Player clock (ms) at which a frame was handed to the player for output.
  void AddOutput(double t_ms) {
    if (outputs_.size() < kMaxFrames) outputs_.push_back(t_ms);
  }
  // Duration of one aom_codec_decode call.
  void AddDecodeCall(double ms) {
    if (decode_calls_.size() < kMaxFrames) decode_calls_.push_back(ms);
  }
  size_t Frames() const { return frames_.size(); }
  size_t Outputs() const { return outputs_.size(); }

  // Number of leading samples left out of the statistics.
  static size_t Warmup(size_t n) { return n > 2 * (size_t)kWarmupFrames ? (size_t)kWarmupFrames : 0; }

  static double Blocked(const av1_perf_frame& f) {
    double s = 0.0;
    for (size_t i = 0; i < sizeof(kBlockedIds) / sizeof(kBlockedIds[0]); ++i) s += f.cpu_ms[kBlockedIds[i]];
    return s;
  }

  std::vector<std::string> Summary(const OutputWaits& waits) const {
    std::vector<std::string> out;
    char b[512];
    const size_t n = frames_.size();
    const size_t w = Warmup(n);
    const size_t ow = Warmup(outputs_.size());

    // The records of show_existing_frame copies are kept out of the per-frame statistics: such a record is
    // an output copy of a few milliseconds, not a decoded frame, and a stream that shows every second frame
    // that way would halve every mean. They get a line of their own and are in the per-OUTPUT line.
    size_t decoded_total = 0, decoded_warmup = 0;
    for (size_t i = 0; i < n; ++i) {
      if (frames_[i].is_sef) continue;
      ++decoded_total;
      if (i < w) ++decoded_warmup;
    }
    const size_t sef_total = n - decoded_total;
    size_t shown = 0, hidden = 0, sef = 0, intra = 0, gpu_ok = 0, sec_slot = 0;
    Series wall, busy, sum, gap, blocked, iters, mbytes, abytes, wall_out, calls;
    Series sef_busy, sef_frame, sef_blocked;
    // Completion time of the decoded frame before the current one, warm-up included.
    double prev_complete = 0.0;
    bool have_prev = false;
    for (size_t i = 0; i < w && i < n; ++i) {
      if (frames_[i].is_sef) continue;
      prev_complete = frames_[i].complete_ms;
      have_prev = true;
    }
    Series intra_dispatches, intra_barriers, intra_groups, intra_passes;
    size_t intra_v2 = 0;
    Series gpu_stage[AV1_PERF_GPU_STAGE_COUNT];
    Series cpu[AV1_PERF_CPU_COUNT];
    double stage_total[AV1_PERF_GPU_STAGE_COUNT] = {0.0};
    for (size_t i = w; i < n; ++i) {
      const av1_perf_frame& f = frames_[i];
      if (f.is_sef) {
        ++sef;
        if (f.gpu_valid) sef_busy.Add(f.gpu_span_ms);
        sef_frame.Add(f.cpu_ms[AV1_PERF_CPU_FRAME]);
        sef_blocked.Add(Blocked(f));
        continue;
      }
      if (f.show_frame)
        ++shown;
      else
        ++hidden;
      if (f.intra_frame) ++intra;
      if (f.used_sec_slot) ++sec_slot;
      if (have_prev) wall.Add(f.complete_ms - prev_complete);
      prev_complete = f.complete_ms;
      have_prev = true;
      if (f.gpu_valid) {
        ++gpu_ok;
        busy.Add(f.gpu_span_ms);
        sum.Add(f.gpu_sum_ms);
        if (f.gpu_gap_ms >= 0.0 && i > 0) gap.Add(f.gpu_gap_ms);
        for (int s = 0; s < AV1_PERF_GPU_STAGE_COUNT; ++s) {
          if (f.gpu_stage_hits[s] > 0) {
            gpu_stage[s].Add(f.gpu_stage_ms[s]);
            stage_total[s] += f.gpu_stage_ms[s];
          }
        }
      }
      for (int c = 0; c < AV1_PERF_CPU_COUNT; ++c) cpu[c].Add(f.cpu_ms[c]);
      blocked.Add(Blocked(f));
      if (f.intra_iters >= 0) {
        iters.Add((double)f.intra_iters);
        intra_dispatches.Add((double)f.intra_dispatches);
        intra_barriers.Add((double)f.intra_barriers);
        intra_groups.Add((double)f.intra_groups);
        intra_passes.Add((double)f.intra_passes);
        if (f.intra_v2) ++intra_v2;
      }
      mbytes.Add((double)f.memset_bytes / (1024.0 * 1024.0));
      abytes.Add((double)f.async_clear_bytes / (1024.0 * 1024.0));
    }
    for (size_t i = (ow > 0 ? ow : 1); i < outputs_.size(); ++i) wall_out.Add(outputs_[i] - outputs_[i - 1]);
    for (size_t i = Warmup(decode_calls_.size()); i < decode_calls_.size(); ++i) calls.Add(decode_calls_[i]);

    const size_t measured = decoded_total - decoded_warmup;
    snprintf(b, sizeof(b),
             "==== summary: %u frames decoded, %u output; warm-up %u skipped, %u measured "
             "(%u shown, %u hidden, %u show-existing copies, %u intra), GPU timestamps on %u ====",
             (unsigned)decoded_total, (unsigned)outputs_.size(), (unsigned)decoded_warmup, (unsigned)measured,
             (unsigned)shown, (unsigned)hidden, (unsigned)sef, (unsigned)intra, (unsigned)gpu_ok);
    out.push_back(b);
    if (sef_total) {
      // An output that is not a shown frame of its own: the copy of a frame decoded earlier (and hidden then).
      snprintf(b, sizeof(b),
               "show-existing copies: %u (%u measured): gpu busy mean %.3f ms, cpu decode thread mean %.3f ms, "
               "blocked mean %.3f ms; not in the per-frame lines below",
               (unsigned)sef_total, (unsigned)sef, sef_busy.Mean(), sef_frame.Mean(), sef_blocked.Mean());
      out.push_back(b);
    }
    if (n == 0) {
      out.push_back("no frame records: nothing was decoded, or the decoder never reached a GPU submit");
      return out;
    }
    if (w == 0) out.push_back("too few frames for a warm-up, all frames are in the statistics");
    if (dropped_) {
      snprintf(b, sizeof(b), "%u later frames not recorded (record limit %d)", (unsigned)dropped_, (int)kMaxFrames);
      out.push_back(b);
    }
    if (gpu_ok == 0)
      out.push_back("NO GPU TIMESTAMPS: the decoder queue refused timestamp queries, gpu lines below are empty");

    snprintf(b, sizeof(b), "target: 24 fps needs <= %.2f ms per output frame", kTargetMs);
    out.push_back(b);
    out.push_back("-- per frame, ms --                          n     mean      p50      p95      max");
    Line(&out, "wall between output frames", wall_out, Fps(wall_out.Mean()), "fps output");
    Line(&out, "wall between decoded frames", wall, Fps(wall.Mean()), "fps decoded");
    Line(&out, "gpu busy (first to last timestamp)", busy, Pct(busy.Sum(), wall.Sum()), "% of wall");
    Line(&out, "gpu stage sum", sum, 0.0, 0);
    Line(&out, "gpu idle gap before the frame", gap, 0.0, 0);
    Line(&out, "cpu decode thread, setup to submit", cpu[AV1_PERF_CPU_FRAME], Pct(cpu[AV1_PERF_CPU_FRAME].Sum(), wall.Sum()),
         "% of wall");
    Line(&out, "cpu entropy/tile decode, wall", cpu[AV1_PERF_CPU_ENTROPY_WALL], 0.0, 0);
    Line(&out, "cpu entropy/tile decode, thread sum", cpu[AV1_PERF_CPU_ENTROPY_SUM], 0.0, 0);
    // With rav1d's parse: parts of the thread sum. The block lists (counted superblock by superblock, or
    // with -parseopt latelists at the end of a tile); with -parseopt time the other calls of the sink. What is left of the thread sum is rav1d with the coefficients it writes itself. All zero with
    // the entropy decoder of libaom (-parser c), which has no timers for them.
    Line(&out, "cpu parse: block lists", cpu[AV1_PERF_CPU_SINK_LISTS], 0.0, 0);
    Line(&out, "cpu parse: sink calls (-parseopt time)", cpu[AV1_PERF_CPU_SINK_CALLS], 0.0, 0);
    {
      // The thread sum less the two: only with -parseopt time is this rav1d by itself.
      Series rest;
      for (size_t i = w; i < n; ++i) {
        const av1_perf_frame& f = frames_[i];
        if (f.is_sef) continue;
        rest.Add(f.cpu_ms[AV1_PERF_CPU_ENTROPY_SUM] - f.cpu_ms[AV1_PERF_CPU_SINK_LISTS] -
                 f.cpu_ms[AV1_PERF_CPU_SINK_CALLS]);
      }
      Line(&out, "cpu parse: thread sum less the above", rest, 0.0, 0);
    }
    Line(&out, "cpu command list record", cpu[AV1_PERF_CPU_RECORD], 0.0, 0);
    Line(&out, "cpu submit", cpu[AV1_PERF_CPU_SUBMIT], 0.0, 0);
    Line(&out, "cpu memsets (frame + tile)", SumOf(w, AV1_PERF_CPU_MEMSET, AV1_PERF_CPU_MEMSET_TILE), mbytes.Mean(),
         "MB cleared per frame");
    // The clears on the completion thread, where they are by default. Zero with -syncclear.
    Line(&out, "cpu clears, completion thread", cpu[AV1_PERF_CPU_ASYNC_CLEAR], abytes.Mean(),
         "MB cleared per frame");
    Line(&out, "blocked, decode thread, all waits", blocked, Pct(blocked.Sum(), wall.Sum()), "% of wall");
    Line(&out, "blocked on fence, completion thread", cpu[AV1_PERF_CPU_FENCE_WAIT], 0.0, 0);
    Line(&out, "decode call (aom_codec_decode)", calls, 0.0, 0);

    // What an output frame costs: hidden frames are paid for by the shown ones, and so are the copies of the
    // frames shown with show_existing_frame.
    if (!outputs_.empty()) {
      const double ratio = (double)decoded_total / (double)outputs_.size();
      const double sef_ratio = (double)sef_total / (double)outputs_.size();
      snprintf(b, sizeof(b),
               "per OUTPUT frame (%.2f decoded frames each): wall %.2f ms, gpu busy %.2f ms, cpu decode thread %.2f ms, "
               "entropy wall %.2f ms, blocked %.2f ms (target %.2f)",
               ratio, wall_out.Mean(), busy.Mean() * ratio + sef_busy.Mean() * sef_ratio,
               cpu[AV1_PERF_CPU_FRAME].Mean() * ratio + sef_frame.Mean() * sef_ratio,
               cpu[AV1_PERF_CPU_ENTROPY_WALL].Mean() * ratio, blocked.Mean() * ratio + sef_blocked.Mean() * sef_ratio,
               kTargetMs);
      out.push_back(b);
    }

    out.push_back("-- gpu stages, ms (frames = frames that ran the stage; avg = spread over all measured frames) --");
    out.push_back("stage                    frames     mean      p50      p95      max      avg   share");
    for (int s = 0; s < AV1_PERF_GPU_STAGE_COUNT; ++s) {
      const Series& g = gpu_stage[s];
      if (g.Count() == 0) {
        snprintf(b, sizeof(b), "gpu %-18s %8u  (not dispatched)", kGpuStageNames[s], 0u);
      } else {
        snprintf(b, sizeof(b), "gpu %-18s %8u %8.3f %8.3f %8.3f %8.3f %8.3f %6.1f%%", kGpuStageNames[s],
                 (unsigned)g.Count(), g.Mean(), g.Percentile(50), g.Percentile(95), g.Max(),
                 gpu_ok ? stage_total[s] / (double)gpu_ok : 0.0, Pct(stage_total[s], sum.Sum()));
      }
      out.push_back(b);
    }

    out.push_back("-- cpu timers, ms per frame --                n     mean      p50      p95      max");
    for (int c = 0; c < AV1_PERF_CPU_COUNT; ++c) {
      char name[64];
      snprintf(name, sizeof(name), "cpu %s", kCpuNames[c]);
      Line(&out, name, cpu[c], 0.0, 0);
    }

    snprintf(b, sizeof(b), "intra wavefront iterations per frame: mean %.1f, max %.0f (%u frames with intra)",
             iters.Mean(), iters.Max(), (unsigned)iters.Count());
    out.push_back(b);
    // The cost drivers of the intra stage. With -intrav2 an "iteration" above is a step of the new schedule.
    snprintf(b, sizeof(b),
             "intra stage per frame with intra: dispatches mean %.1f max %.0f, UAV barriers mean %.1f max %.0f, "
             "thread groups mean %.0f max %.0f, passes per group %.1f (intra_v2 on %u of %u frames)",
             intra_dispatches.Mean(), intra_dispatches.Max(), intra_barriers.Mean(), intra_barriers.Max(),
             intra_groups.Mean(), intra_groups.Max(),
             intra_groups.Sum() > 0.0 ? intra_passes.Sum() / intra_groups.Sum() : 0.0, (unsigned)intra_v2,
             (unsigned)iters.Count());
    out.push_back(b);
    snprintf(b, sizeof(b), "frames that also took a second pipeline slot: %u of %u", (unsigned)sec_slot,
             (unsigned)measured);
    out.push_back(b);
    snprintf(b, sizeof(b), "output-buffer waits: %d, total %.1f ms, longest %.1f ms (whole run, warm-up included)",
             waits.count, waits.total_ms, waits.max_ms);
    out.push_back(b);
    InterSummary(&out, w);
    out.push_back("==== end of summary ====");
    return out;
  }

  // One line per record (a decoded frame, or a show_existing_frame copy with sef=1), for the -progress dump
  // after the summary.
  std::vector<std::string> FrameLines() const {
    std::vector<std::string> out;
    char b[1536];
    for (size_t i = 0; i < frames_.size(); ++i) {
      const av1_perf_frame& f = frames_[i];
      int len = snprintf(b, sizeof(b),
                         "frame %u no=%d show=%d sef=%d intra=%d iters=%d iv2=%d idisp=%d ibarr=%d igroups=%d "
                         "ipasses=%d tiles=%d sec=%d wall=%.2f busy=%.2f gap=%.2f |",
                         (unsigned)i, f.frame_number, f.show_frame, f.is_sef, f.intra_frame, f.intra_iters,
                         f.intra_v2, f.intra_dispatches, f.intra_barriers, f.intra_groups, f.intra_passes,
                         f.tile_count, f.used_sec_slot, i > 0 ? f.complete_ms - frames_[i - 1].complete_ms : 0.0,
                         f.gpu_span_ms, f.gpu_gap_ms);
      for (int s = 0; s < AV1_PERF_GPU_STAGE_COUNT; ++s) {
        if (f.gpu_stage_hits[s] > 0) Append(b, sizeof(b), &len, " %s=%.2f", kGpuStageNames[s], f.gpu_stage_ms[s]);
      }
      Append(b, sizeof(b), &len, " |");
      for (int c = 0; c < AV1_PERF_CPU_COUNT; ++c) {
        if (f.cpu_ms[c] >= 0.005) Append(b, sizeof(b), &len, " %s=%.2f", kCpuNames[c], f.cpu_ms[c]);
      }
      out.push_back(b);
      // -gputimedetail: the frame's inter breakdown, a line of its own behind the frame's line. Per section
      // name=ms:thread groups:block records; the passes as p1..p3.
      const InterFrame* d = InterOf(i);
      if (d && (d->has_inter || d->sections[AV1_PERF_INTER_RECON].dispatches)) {
        len = snprintf(b, sizeof(b), "frame %u inter mode=%d scaled=%d blocks=%lld p1=%.3f p2=%.3f p3=%.3f |", (unsigned)i,
                       d->mode, d->scaled, (long long)d->blocks.blocks, d->PassMs(0), d->PassMs(1), d->PassMs(2));
        for (int k = 0; k < AV1_PERF_INTER_SECTION_COUNT; ++k) {
          const InterFrame::Section& sc = d->sections[k];
          if (!sc.timed && !sc.dispatches) continue;
          if (InterIsSync(k))
            Append(b, sizeof(b), &len, " %s=%.3f", kInterSectionNames[k], sc.ms);
          else
            Append(b, sizeof(b), &len, " %s=%.3f:%d:%lld", kInterSectionNames[k], sc.ms, sc.groups,
                   (long long)sc.records);
        }
        out.push_back(b);
      }
    }
    return out;
  }

 private:
  static double Fps(double ms) { return ms > 0.0 ? 1000.0 / ms : 0.0; }
  static double Pct(double part, double whole) { return whole > 0.0 ? 100.0 * part / whole : 0.0; }

  Series SumOf(size_t from, int id_a, int id_b) const {
    Series s;
    for (size_t i = from; i < frames_.size(); ++i) {
      if (frames_[i].is_sef) continue;
      s.Add(frames_[i].cpu_ms[id_a] + frames_[i].cpu_ms[id_b]);
    }
    return s;
  }

  static void Line(std::vector<std::string>* out, const char* name, const Series& s, double extra,
                   const char* extra_name) {
    char b[512];
    int len = snprintf(b, sizeof(b), "%-38s %6u %8.3f %8.3f %8.3f %8.3f", name, (unsigned)s.Count(), s.Mean(),
                       s.Percentile(50), s.Percentile(95), s.Max());
    if (extra_name) Append(b, sizeof(b), &len, "   %.1f %s", extra, extra_name);
    out->push_back(b);
  }

  static void Append(char* b, size_t cap, int* len, const char* fmt, ...) {
    if (*len < 0 || (size_t)*len >= cap) return;
    va_list ap;
    va_start(ap, fmt);
    const int r = vsnprintf(b + *len, cap - (size_t)*len, fmt, ap);
    va_end(ap);
    if (r > 0) *len += r;
  }

  const InterFrame* InterOf(size_t frame) const {
    if (frame >= inter_index_.size() || inter_index_[frame] < 0) return NULL;
    return &inter_[(size_t)inter_index_[frame]];
  }

  static double Mb(double bytes) { return bytes / (1024.0 * 1024.0); }

  // What the frames of a run did in one section (or in one of its dispatch sizes).
  struct InterSum {
    Series ms;  // per frame in which it was timed
    size_t frames;  // frames in which it dispatched something
    double dispatches, groups, threads, records, pixels, loaded;
    double work[sizeof(av1_perf_inter_work) / sizeof(int64_t)];
    InterSum() : frames(0), dispatches(0), groups(0), threads(0), records(0), pixels(0), loaded(0) {
      for (size_t i = 0; i < sizeof(work) / sizeof(work[0]); ++i) work[i] = 0.0;
    }
  };

  // The inter breakdown (-gputimedetail), appended to the summary when a frame of the run has one. |w|: the
  // warm-up records, as for the lines above.
  void InterSummary(std::vector<std::string>* out, size_t w) const {
    char b[640];
    const size_t n = frames_.size();
    size_t with_detail = 0, inter_frames = 0, scaled_frames = 0, mode2 = 0, disagree = 0;
    char first_mismatch[280] = "";
    // [section][scaled]
    InterSum sum[AV1_PERF_INTER_SECTION_COUNT][2];
    Series pass[3], inter_total, inter_stage, barriers, marks;
    const size_t blocks_fields = sizeof(av1_perf_inter_blocks) / sizeof(int64_t);
    std::vector<double> blocks_sum(blocks_fields, 0.0);
    double bytes_per_sample = 0.0;
    struct ListKey {
      int section, scaled, width, height;
      bool operator<(const ListKey& o) const {
        if (section != o.section) return section < o.section;
        if (scaled != o.scaled) return scaled < o.scaled;
        if (width * height != o.width * o.height) return width * height < o.width * o.height;
        return width < o.width;
      }
    };
    std::map<ListKey, InterSum> lists;
    for (size_t i = w; i < n; ++i) {
      const av1_perf_frame& f = frames_[i];
      const InterFrame* d = InterOf(i);
      if (f.is_sef || !d) continue;
      ++with_detail;
      if (d->mode == 2) ++mode2;
      const int sc = d->scaled ? 1 : 0;
      for (int k = 0; k < AV1_PERF_INTER_SECTION_COUNT; ++k) {
        const InterFrame::Section& src = d->sections[k];
        if (InterPassOf(k) >= 0 && !d->has_inter) continue;
        // The barriers and the reconstruction are the same for a frame with scaled references.
        InterSum& dst = sum[k][InterPassOf(k) < 0 || InterIsSync(k) ? 0 : sc];
        if (src.timed) dst.ms.Add(src.ms);
        if (!src.dispatches) continue;
        ++dst.frames;
        dst.dispatches += src.dispatches;
        dst.groups += src.groups;
        dst.threads += (double)src.threads;
        dst.records += (double)src.records;
        dst.pixels += (double)src.pixels;
        dst.loaded += (double)src.loaded_bytes;
        const int64_t* wk = (const int64_t*)&src.work;
        for (size_t j = 0; j < sizeof(dst.work) / sizeof(dst.work[0]); ++j) dst.work[j] += (double)wk[j];
      }
      for (size_t l = 0; l < d->lists.size(); ++l) {
        const InterFrame::List& item = d->lists[l];
        if (InterPassOf(item.section) >= 0 && !d->has_inter) continue;
        const ListKey key = {item.section, InterPassOf(item.section) < 0 ? 0 : sc, item.list.width, item.list.height};
        InterSum& dst = lists[key];
        ++dst.frames;
        dst.ms.Add(item.list.ms);
        dst.dispatches += 1;
        dst.groups += item.list.groups;
        dst.threads += item.list.threads;
        dst.records += item.list.records;
        dst.pixels += (double)item.list.records * item.list.width * item.list.height;
      }
      barriers.Add((double)d->barriers_added);
      marks.Add((double)d->marks);
      if (!d->has_inter) continue;
      const std::string mismatch = d->Mismatch();
      if (!mismatch.empty()) {
        if (!disagree) snprintf(first_mismatch, sizeof(first_mismatch), "record %u, %s", (unsigned)i, mismatch.c_str());
        ++disagree;
      }
      ++inter_frames;
      if (d->scaled) ++scaled_frames;
      if (f.gpu_valid)
        for (int p = 0; p < 3; ++p) pass[p].Add(d->PassMs(p));
      // The counters need no GPU timestamps; the times do.
      if (f.gpu_valid) {
        inter_total.Add(d->InterMs());
        inter_stage.Add(f.gpu_stage_ms[AV1_PERF_GPU_INTER]);
      }
      const int64_t* bl = (const int64_t*)&d->blocks;
      for (size_t j = 0; j < blocks_fields; ++j) blocks_sum[j] += (double)bl[j];
      bytes_per_sample = (double)d->bytes_per_sample;
    }
    if (!with_detail) return;
    const bool isolate = mode2 > 0;
    const double inter_sum = inter_total.Sum();

    snprintf(b, sizeof(b),
             "-- inter breakdown (%s): %u frames with an inter stage of %u measured with the breakdown, %u of them "
             "with scaled references --",
             isolate ? "-gputimeisolate: a UAV barrier and a timestamp behind every dispatch" : "-gputimedetail",
             (unsigned)inter_frames, (unsigned)with_detail, (unsigned)scaled_frames);
    out->push_back(b);
    if (isolate) {
      out->push_back("excl: every dispatch ran alone between two UAV barriers, so all times below are exclusive. The "
                     "barriers and timestamps cost time themselves:");
      snprintf(b, sizeof(b),
               "isolate mode added per frame: UAV barriers mean %.1f max %.0f, timestamps mean %.1f max %.0f; its cost "
               "is 'gpu inter' + 'gpu recon' of this run minus those of a run with plain -gputime",
               barriers.Mean(), barriers.Max(), marks.Mean(), marks.Max());
      out->push_back(b);
    } else {
      out->push_back("excl: between two UAV barriers, an exclusive time (the passes, recon). ovl: the dispatches of a "
                     "pass run side by side, a section's time");
      out->push_back("is from the timestamp before it to its own: submission order, overlapping with its neighbours "
                     "in the pass. sync: the wait at the pass's barrier.");
      snprintf(b, sizeof(b), "timestamps added per frame: mean %.1f max %.0f (no barriers added)", marks.Mean(),
               marks.Max());
      out->push_back(b);
    }
    // The counters of the two sides are independent: the dispatches from the lists the recording walks, the
    // blocks from the tile threads.
    if (disagree)
      snprintf(b, sizeof(b), "inter counters DISAGREE on %u of %u frames; first: %s", (unsigned)disagree,
               (unsigned)inter_frames, first_mismatch);
    else
      snprintf(b, sizeof(b),
               "inter counters: the dispatches and the tile threads agree on all %u frames (block records, samples, "
               "luma area)",
               (unsigned)inter_frames);
    out->push_back(b);
    out->push_back("-- inter times, ms (frames = frames timed; avg = spread over the frames with an inter stage; share "
                   "= of the inter stage) --");
    out->push_back("section                  kind   frames     mean      p50      p95      max      avg   share");
    for (int k = 0; k < AV1_PERF_INTER_SECTION_COUNT; ++k) {
      for (int sc = 0; sc < 2; ++sc) {
        const InterSum& g = sum[k][sc];
        if (g.ms.Count() == 0) continue;
        char name[48];
        snprintf(name, sizeof(name), "%s%s", kInterSectionNames[k], sc ? "/scaled" : "");
        const bool recon = k == AV1_PERF_INTER_RECON;
        const char* kind = InterIsSync(k) ? "sync" : (isolate || recon) ? "excl" : "ovl";
        InterTimeLine(out, name, kind, g.ms, recon ? (double)with_detail : (double)inter_frames, recon ? 0.0 : inter_sum,
                      !recon);
      }
      // The pass ends behind its barrier.
      if (InterIsSync(k)) {
        char name[48];
        const int p = InterPassOf(k);
        snprintf(name, sizeof(name), "pass %d (to %s)", p + 1, kInterSectionNames[k]);
        InterTimeLine(out, name, "excl", pass[p], (double)inter_frames, inter_sum, true);
      }
    }
    InterTimeLine(out, "passes 1 to 3", "excl", inter_total, (double)inter_frames, inter_sum, true);
    InterTimeLine(out, "stage (gpu inter)", "excl", inter_stage, (double)inter_frames, inter_sum, true);

    out->push_back("-- inter work per frame that has the section (means); time per sample and bytes per second from "
                   "the section's time above, exclusive only where it says excl --");
    out->push_back("section                  frames  disp  groups  full%  records blocksY blocksUV   kpxY  kpxUV refblk "
                   " loadMB footMB  mvMB  ns/px   GB/s");
    for (int k = 0; k < AV1_PERF_INTER_SECTION_COUNT; ++k) {
      if (InterIsSync(k)) continue;
      for (int sc = 0; sc < 2; ++sc) {
        const InterSum& g = sum[k][sc];
        if (!g.frames) continue;
        char name[48];
        snprintf(name, sizeof(name), "%s%s", kInterSectionNames[k], sc ? "/scaled" : "");
        const double fr = (double)g.frames;
        const double ms = g.ms.Sum();
        // work[]: blocks_y, blocks_uv, pixels_y, pixels_uv, ref_blocks, footprint, footprint_mv (samples).
        snprintf(b, sizeof(b),
                 "inter %-18s %6u %5.1f %7.0f %6.1f %8.0f %7.0f %8.0f %6.0f %6.0f %6.0f %7.2f %6.2f %5.2f %6.2f %6.2f",
                 name, (unsigned)g.frames, g.dispatches / fr, g.groups / fr,
                 g.groups > 0.0 ? 100.0 * g.threads / (g.groups * 64.0) : 0.0, g.records / fr, g.work[0] / fr,
                 g.work[1] / fr, g.work[2] / fr / 1000.0, g.work[3] / fr / 1000.0, g.work[4] / fr, Mb(g.loaded) / fr,
                 Mb(g.work[5] * bytes_per_sample) / fr, Mb(g.work[6] * bytes_per_sample) / fr,
                 g.pixels > 0.0 ? ms * 1e6 / g.pixels : 0.0, ms > 0.0 ? g.loaded / (ms * 1e-3) / 1e9 : 0.0);
        out->push_back(b);
      }
    }
    out->push_back("disp dispatches; groups thread groups of 64; full% threads with work of the threads dispatched; "
                   "records block records (U and V one each);");
    out->push_back("blocksY/UV prediction blocks; kpxY/UV thousand samples predicted; refblk reference blocks fetched "
                   "(per block, plane and reference); loadMB reference");
    out->push_back("bytes the shader's threads load (rows of an 8-tap filter both ways, whatever the vector); footMB "
                   "the (w+7)x(h+7) areas of the references; mvMB the same with");
    out->push_back("the margin only where the motion vector has a fraction; ns/px section time per sample predicted; "
                   "GB/s loaded bytes per second of section time.");

    if (inter_frames) {
      const double fr = (double)inter_frames;
      // blocks_sum[]: blocks, area, compound, obmc, warp, wedge, diffwtd, interintra, scaled, ref_area,
      // ref_area_mv[4], ref_area_filter[2][3].
      const double area = blocks_sum[1];
      const double ref_area = blocks_sum[9];
      snprintf(b, sizeof(b),
               "inter blocks per frame: %.0f blocks, %.3f Mpx luma; of that area: compound %.1f%%, OBMC %.1f%%, warped "
               "%.1f%%, wedge mask %.1f%%, difference weighted %.1f%%, inter-intra %.1f%%, scaled reference %.1f%%",
               blocks_sum[0] / fr, area / fr / 1e6, Pct(blocks_sum[2], area), Pct(blocks_sum[3], area),
               Pct(blocks_sum[4], area), Pct(blocks_sum[5], area), Pct(blocks_sum[6], area), Pct(blocks_sum[7], area),
               Pct(blocks_sum[8], area));
      out->push_back(b);
      snprintf(b, sizeof(b),
               "inter reference fetches (luma area per reference, %.3f Mpx per frame) by the fraction of the motion "
               "vector: none %.1f%%, horizontal only %.1f%%, vertical only %.1f%%, both %.1f%% (warped and scaled count "
               "as both)",
               ref_area / fr / 1e6, Pct(blocks_sum[10], ref_area), Pct(blocks_sum[11], ref_area),
               Pct(blocks_sum[12], ref_area), Pct(blocks_sum[13], ref_area));
      out->push_back(b);
      snprintf(b, sizeof(b),
               "inter filter kernels asked for (same area): horizontal 8-tap %.1f%%, 4-tap %.1f%%, bilinear %.1f%%; "
               "vertical 8-tap %.1f%%, 4-tap %.1f%%, bilinear %.1f%%",
               Pct(blocks_sum[14], ref_area), Pct(blocks_sum[15], ref_area), Pct(blocks_sum[16], ref_area),
               Pct(blocks_sum[17], ref_area), Pct(blocks_sum[18], ref_area), Pct(blocks_sum[19], ref_area));
      out->push_back(b);
      double loaded = 0.0, foot = 0.0, foot_mv = 0.0, written = 0.0;
      for (int k = 0; k < AV1_PERF_INTER_RECON; ++k)
        for (int sc = 0; sc < 2; ++sc) {
          loaded += sum[k][sc].loaded;
          foot += sum[k][sc].work[5] * bytes_per_sample;
          foot_mv += sum[k][sc].work[6] * bytes_per_sample;
          written += sum[k][sc].pixels * bytes_per_sample;
        }
      snprintf(b, sizeof(b),
               "inter stage per frame: reference bytes loaded %.2f MB, footprint %.2f MB, footprint by motion vector "
               "%.2f MB, samples written %.2f MB; loaded/written %.1f, footprint/written %.1f",
               Mb(loaded) / fr, Mb(foot) / fr, Mb(foot_mv) / fr, Mb(written) / fr, written > 0.0 ? loaded / written : 0.0,
               written > 0.0 ? foot / written : 0.0);
      out->push_back(b);
    }

    out->push_back(isolate ? "-- inter dispatches by block size (frames = frames with the dispatch; ms mean per such "
                             "frame, exclusive; share = of the inter stage) --"
                           : "-- inter dispatches by block size (frames = frames with the dispatch; no times: run "
                             "-gputimeisolate for those) --");
    out->push_back("section                size     frames  records   groups  full%   ms mean    share  ns/px");
    for (std::map<ListKey, InterSum>::const_iterator it = lists.begin(); it != lists.end(); ++it) {
      const ListKey& key = it->first;
      const InterSum& g = it->second;
      char name[48], size[24];
      snprintf(name, sizeof(name), "%s%s", kInterSectionNames[key.section], key.scaled ? "/scaled" : "");
      snprintf(size, sizeof(size), "%dx%d", key.width, key.height);
      const double fr = (double)g.frames;
      int len = snprintf(b, sizeof(b), "list %-17s %-8s %6u %8.0f %8.0f %6.1f", name, size, (unsigned)g.frames,
                         g.records / fr, g.groups / fr, g.groups > 0.0 ? 100.0 * g.threads / (g.groups * 64.0) : 0.0);
      if (isolate)
        Append(b, sizeof(b), &len, " %9.4f %7.2f%% %6.2f", g.ms.Mean(),
               key.section == AV1_PERF_INTER_RECON ? 0.0 : Pct(g.ms.Sum(), inter_sum),
               g.pixels > 0.0 ? g.ms.Sum() * 1e6 / g.pixels : 0.0);
      out->push_back(b);
    }
  }

  static void InterTimeLine(std::vector<std::string>* out, const char* name, const char* kind, const Series& s,
                            double frames, double inter_sum, bool share) {
    char b[256];
    int len = snprintf(b, sizeof(b), "inter %-18s %-5s %7u %8.3f %8.3f %8.3f %8.3f %8.3f", name, kind, (unsigned)s.Count(),
                       s.Mean(), s.Percentile(50), s.Percentile(95), s.Max(), frames > 0.0 ? s.Sum() / frames : 0.0);
    if (share) Append(b, sizeof(b), &len, " %6.1f%%", Pct(s.Sum(), inter_sum));
    out->push_back(b);
  }

  std::vector<av1_perf_frame> frames_;
  std::vector<double> outputs_;
  std::vector<double> decode_calls_;
  // -gputimedetail: the inter breakdowns of the frames that have one, and for every frame record its place in
  // inter_ (-1: none).
  std::vector<InterFrame> inter_;
  std::vector<int> inter_index_;
  size_t dropped_ = 0;
};

}  // namespace perfstats
