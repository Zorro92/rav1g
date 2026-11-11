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
    "rec_copy",      "submit",        "frame",          "fence_wait",     "gpu_latency", "notify"};
static_assert(sizeof(kCpuNames) / sizeof(kCpuNames[0]) == AV1_PERF_CPU_COUNT,
              "CPU timer names out of step with av1_perf.h");

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
    dropped_ = 0;
  }
  // One record per decoded frame, in completion order.
  void AddFrame(const av1_perf_frame& f) {
    if (frames_.size() >= kMaxFrames) {
      ++dropped_;
      return;
    }
    frames_.push_back(f);
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

    size_t shown = 0, hidden = 0, sef = 0, intra = 0, gpu_ok = 0, sec_slot = 0;
    Series wall, busy, sum, gap, blocked, iters, mbytes, wall_out, calls;
    Series gpu_stage[AV1_PERF_GPU_STAGE_COUNT];
    Series cpu[AV1_PERF_CPU_COUNT];
    double stage_total[AV1_PERF_GPU_STAGE_COUNT] = {0.0};
    for (size_t i = w; i < n; ++i) {
      const av1_perf_frame& f = frames_[i];
      if (f.is_sef)
        ++sef;
      else if (f.show_frame)
        ++shown;
      else
        ++hidden;
      if (f.intra_frame) ++intra;
      if (f.used_sec_slot) ++sec_slot;
      if (i > 0) wall.Add(f.complete_ms - frames_[i - 1].complete_ms);
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
      if (f.intra_iters >= 0) iters.Add((double)f.intra_iters);
      mbytes.Add((double)f.memset_bytes / (1024.0 * 1024.0));
    }
    for (size_t i = (ow > 0 ? ow : 1); i < outputs_.size(); ++i) wall_out.Add(outputs_[i] - outputs_[i - 1]);
    for (size_t i = Warmup(decode_calls_.size()); i < decode_calls_.size(); ++i) calls.Add(decode_calls_[i]);

    const size_t measured = n - w;
    snprintf(b, sizeof(b),
             "==== summary: %u frames decoded, %u output; warm-up %u skipped, %u measured "
             "(%u shown, %u hidden, %u show-existing copies, %u intra), GPU timestamps on %u ====",
             (unsigned)n, (unsigned)outputs_.size(), (unsigned)w, (unsigned)measured, (unsigned)shown,
             (unsigned)hidden, (unsigned)sef, (unsigned)intra, (unsigned)gpu_ok);
    out.push_back(b);
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
    Line(&out, "cpu command list record", cpu[AV1_PERF_CPU_RECORD], 0.0, 0);
    Line(&out, "cpu submit", cpu[AV1_PERF_CPU_SUBMIT], 0.0, 0);
    Line(&out, "cpu memsets (frame + tile)", SumOf(w, AV1_PERF_CPU_MEMSET, AV1_PERF_CPU_MEMSET_TILE), mbytes.Mean(),
         "MB cleared per frame");
    Line(&out, "blocked, decode thread, all waits", blocked, Pct(blocked.Sum(), wall.Sum()), "% of wall");
    Line(&out, "blocked on fence, completion thread", cpu[AV1_PERF_CPU_FENCE_WAIT], 0.0, 0);
    Line(&out, "decode call (aom_codec_decode)", calls, 0.0, 0);

    // What an output frame costs: hidden frames are paid for by the shown ones.
    if (!outputs_.empty()) {
      const double ratio = (double)n / (double)outputs_.size();
      snprintf(b, sizeof(b),
               "per OUTPUT frame (%.2f decoded frames each): wall %.2f ms, gpu busy %.2f ms, cpu decode thread %.2f ms, "
               "entropy wall %.2f ms, blocked %.2f ms (target %.2f)",
               ratio, wall_out.Mean(), busy.Mean() * ratio, cpu[AV1_PERF_CPU_FRAME].Mean() * ratio,
               cpu[AV1_PERF_CPU_ENTROPY_WALL].Mean() * ratio, blocked.Mean() * ratio, kTargetMs);
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
    snprintf(b, sizeof(b), "frames that also took the second pipeline slot: %u of %u", (unsigned)sec_slot,
             (unsigned)measured);
    out.push_back(b);
    snprintf(b, sizeof(b), "output-buffer waits: %d, total %.1f ms, longest %.1f ms (whole run, warm-up included)",
             waits.count, waits.total_ms, waits.max_ms);
    out.push_back(b);
    out.push_back("==== end of summary ====");
    return out;
  }

  // One line per decoded frame, for the -progress dump after the summary.
  std::vector<std::string> FrameLines() const {
    std::vector<std::string> out;
    char b[1536];
    for (size_t i = 0; i < frames_.size(); ++i) {
      const av1_perf_frame& f = frames_[i];
      int len = snprintf(b, sizeof(b),
                         "frame %u no=%d show=%d sef=%d intra=%d iters=%d tiles=%d sec=%d wall=%.2f busy=%.2f gap=%.2f |",
                         (unsigned)i, f.frame_number, f.show_frame, f.is_sef, f.intra_frame, f.intra_iters,
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
    }
    return out;
  }

 private:
  static double Fps(double ms) { return ms > 0.0 ? 1000.0 / ms : 0.0; }
  static double Pct(double part, double whole) { return whole > 0.0 ? 100.0 * part / whole : 0.0; }

  Series SumOf(size_t from, int id_a, int id_b) const {
    Series s;
    for (size_t i = from; i < frames_.size(); ++i) s.Add(frames_[i].cpu_ms[id_a] + frames_[i].cpu_ms[id_b]);
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

  std::vector<av1_perf_frame> frames_;
  std::vector<double> outputs_;
  std::vector<double> decode_calls_;
  size_t dropped_ = 0;
};

}  // namespace perfstats
