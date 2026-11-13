/*
 * warp_decoder: the decoder as a desktop console program, for conformance runs.
 *
 * Decodes AV1 streams (IVF, WebM/Matroska, OBU, MP4: testing/common/stream_reader) with libav1 on a
 * D3D12 device, by default the WARP software adapter, reads every output frame back and computes one md5
 * per output frame, the way the test player does (testing/player_common/decoder.cpp): the same decoder
 * configuration and defaults (CDEF v2, a decoder sized for the stream, the shared back buffer), the same
 * output buffer callbacks, the same hash. The md5s are written in the player's format and/or compared with
 * a reference file. No window, no swap chain, nothing of UWP: it runs where there is no GPU, such as a CI
 * runner.
 *
 * Usage, flags and exit codes: docs/warp-decoder.md, or run it without arguments.
 */

#define PSAPI_VERSION 2  // GetProcessMemoryInfo from kernel32
#include <windows.h>
#include <psapi.h>
#include <d3d12.h>
#include <d3d12sdklayers.h>
#include <dxgi1_4.h>
#include <wrl/client.h>

#include <stdarg.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include <chrono>
#include <condition_variable>
#include <fstream>
#include <memory>
#include <mutex>
#include <string>
#include <vector>

#include "common/stream_reader.h"
#include "common/md5_utils.h"
#include "common/stage_dump.h"
#include "aom/aom_codec.h"
#include "aom/aom_decoder.h"
#include "aom/aomdx.h"
#include "dx/av1_perf.h"
#include "player_common/perf_stats.h"
#include "sm6test/sm6test.h"

using Microsoft::WRL::ComPtr;

// The player's values (testing/player_common/cmd_parser.h, decoder_creator.h).
#define WD_SLOTS_DEFAULT 2
#define WD_SLOTS_MIN 2
#define WD_SLOTS_MAX 4
#define WD_LOWMEM_ALL 7
#define WD_LOWMEM_DEFAULT 1
#define WD_DECODER_WIDTH 4096
#define WD_DECODER_HEIGHT 2176
#define WD_DECODER_BITDEPTH 10
#define WD_PSO_THREADS 4
#define WD_QUEUE_SIZE_DEFAULT 4
#define WD_QUEUE_SIZE_MAX 16

// Exit codes of the process.
enum {
  kExitPass = 0,      // every task decoded, and every frame of every task with a reference matched
  kExitMismatch = 1,  // at least one task with a frame that differs from its reference, or a frame count
  kExitError = 2,     // a task could not be decoded (the run stops at that task)
  kExitUsage = 3,     // bad arguments, no D3D12 device, no pipeline states
  kExitTimeout = 4    // a task ran longer than -timeout (the run stops at that task)
};

// tools_common.c wants this from the program it is linked into.
extern "C" void usage_exit(void) { exit(kExitUsage); }

// player_smoke.cpp
int PlayerSmoke(ID3D12Device* device, const char* root, int timeout_s);

struct Task {
  std::string input;
  std::string name;           // -name, or the file name of the input: what the result line calls the stream
  std::string check;          // -check: reference md5 file
  bool md5_frame_check = false;  // -md5_frame_check: the reference is <input>.md5, as in the player
  std::string save_md5;       // -save_md5
  std::string out_yuv;        // -o
  std::string save_wrong_yuv; // -save_wrong_yuv <dir>
  int wrong_limit = -1;       // -wrong_limit: at most this many frames per task for -save_wrong_yuv
  bool aom_md5 = false;       // -aom_md5: monochrome frames are hashed as aomdec --i420 writes them
  int threads = 1;
  int limit = -1;
  int queue_size = WD_QUEUE_SIZE_DEFAULT;
  int slots = WD_SLOTS_DEFAULT;
  bool cdefv1 = false;
  bool cdefv2 = false;
  bool nofitsize = false;
  bool fitsize = false;
  bool lowmem = false;
  int lowmem_parts = WD_LOWMEM_DEFAULT;
  bool intrav2 = false;
  bool intraopt = false;
  bool asyncclear = false;
  int lfhsplit = 0;           // -lfhsplit <0|1|2>
  bool intraedgefix = false;  // -intraedgefix
  int poison = 0;             // -poisonmi (AV1_POISON_MODE_INFO), -poisonbuf (AV1_POISON_BUFFERS)
  int interfast = 0;          // -interfast (1), -interfastlists (7), -interfastmode <n>: av1_set_inter_fast
  bool all_layers = false;    // -alllayers: every shown frame of a temporal unit with spatial layers
  int oppoint = 0;            // -oppoint <n>: the operating point of a stream with layers
  int timeout_s = 0;          // -timeout: seconds for the task, 0 = none
  bool gputime = false;       // -gputime: the timing run of the player, its summary on stdout
  int gputime_detail = 0;     // -gputimedetail (1), -gputimeisolate (2): the inter breakdown; either sets gputime
  bool progress = false;      // -progress: with -gputime, the per-frame lines behind the summary
  std::string dumpstages;     // -dumpstages <frame list>
  std::string dumpdir = "dumps";  // -dumpdir <dir>
  std::vector<int> dump_frames;   // the frames of -dumpstages
};

struct Globals {
  bool hardware = false;  // -hw
  bool verbose = false;   // -v
  bool debug_layer = false;     // -debuglayer
  bool gpu_validation = false;  // -gpuvalidation
  bool sm6test = false;         // -sm6test
  std::string player_smoke;     // -playersmoke <directory>
  std::string list;       // -list
};

static void Say(const char* fmt, ...) {
  va_list ap;
  va_start(ap, fmt);
  vprintf(fmt, ap);
  va_end(ap);
  printf("\n");
  fflush(stdout);
}

// The log callback of the Shader Model 6 test.
static void SayLine(void*, const char* line) { Say("%s", line); }

static void Usage() {
  Say("warp_decoder: decodes AV1 streams with libav1 on D3D12 (WARP by default), one md5 per output frame.");
  Say("");
  Say("  warp_decoder -i <stream> [task flags] [-hw] [-v]");
  Say("  warp_decoder -list <file> [task flags as defaults for every line] [-hw] [-v]");
  Say("  warp_decoder -sm6test [-hw]");
  Say("");
  Say("Task flags:");
  Say("  -i <file>             the stream: .ivf, .mkv/.webm, .obu, .mp4");
  Say("  -name <text>          what the BEGIN and RESULT lines call the stream (default: its file name)");
  Say("  -check <file>         compare with this md5 file: the first 32 characters of each line are the md5 of");
  Say("                        one output frame (the player's format and that of AOM's test vectors)");
  Say("  -md5_frame_check      the same with <stream>.md5, as in the player");
  Say("  -save_md5 <file>      write the md5 of every output frame in the player's format");
  Say("  -aom_md5              hash a monochrome frame with two grey chroma planes, as aomdec --i420 does");
  Say("                        (AOM's .md5 files); without it luma only, as the player and dav1d do");
  Say("  -o <file>             write the visible samples of every output frame (planar, as they are hashed)");
  Say("  -save_wrong_yuv <dir> with a reference: write each frame that differs to <dir>/<name>_<frame>.yuv");
  Say("  -wrong_limit <n>      -save_wrong_yuv writes at most n frames per task");
  Say("  -dumpstages <frames>  write the buffers behind every GPU stage of these frames (decoding order from");
  Say("                        0, hidden frames count; a list like 0,3,10-12) and the lists the stages read");
  Say("                        (mode info, block records, loop filter edge lists) to <dir>/<name>_f<n>.stages,");
  Say("                        as the player does; compare two runs with testing/stage_dump/stage_dump.py");
  Say("  -dumpdir <dir>        where -dumpstages writes (default dumps; created when it is not there)");
  Say("  -limit <n>            decode only the first n temporal units of the stream");
  Say("  -t <n>                decoder threads (default 1)");
  Say("  -queue_size <n>       output images in the pool, 1..%d (default %d)", WD_QUEUE_SIZE_MAX,
      WD_QUEUE_SIZE_DEFAULT);
  Say("  -timeout <seconds>    give up on a task after this time (exit code %d)", kExitTimeout);
  Say("  -cdefv1 -nofitsize -lowmemparts <n> -lowmem -slots <n> -intrav2 -intraopt -asyncclear");
  Say("                        as in the test player (docs/test-player.md); -cdefv2 and -fitsize are accepted");
  Say("  -lfhsplit <n>         horizontal loop filter: 1 even rows of blocks, a UAV barrier, odd rows; 2 every");
  Say("                        row of blocks on its own (default 0: one dispatch per plane; same output)");
  Say("  -intraedgefix         8-bit intra: the above edge filter stops where the 10-bit one does (same output)");
  Say("  -poisonmi             debugging: the mode info pool starts filled with the byte 0xA5 instead of with what");
  Say("                        the memory held, so a shader that reads bytes the decoder never wrote shows in");
  Say("                        the output of any stream (same output)");
  Say("  -poisonbuf            the same for every buffer the CPU writes for the shaders, and the constants of");
  Say("                        every stage filled again in every frame before the stage writes them (same output)");
  Say("  -interfast            inter prediction by the fraction of the motion vector: a copy, one filter pass, or");
  Say("                        both (inter_fast, inter_compound_fast; same output)");
  Say("  -interfastlists       the block lists of -interfast with the original shaders on them (same output)");
  Say("  -interfastmode <n>    av1_set_inter_fast(n): 1 -interfast, +2 original shader for one reference, +4 for two;");
  Say("                        8: only the room for the lists in the block maps, the path stays off (diagnostic)");
  Say("  -gputime              the timing run of the test player: GPU timestamps per stage and CPU timers, the");
  Say("                        summary as 'GPUTIME ...' lines after the task (docs/test-player.md, \"Timing run\");");
  Say("                        on WARP the times say nothing about a GPU, the counters are the stream's");
  Say("  -gputimedetail        -gputime with the inter prediction breakdown: a timestamp behind every section of");
  Say("                        the inter stage, work counters per section");
  Say("  -gputimeisolate       -gputimedetail with a UAV barrier and a timestamp behind every dispatch of the inter");
  Say("                        stage and of the reconstruction (same output)");
  Say("  -progress             with -gputime: one 'GPUTIME frame ...' line per frame behind the summary");
  Say("  -alllayers            a stream with spatial layers: output the frame of every layer of a temporal unit");
  Say("                        (default: one frame per temporal unit, the highest layer it has)");
  Say("  -oppoint <n>          a stream with layers: decode operating point n of its sequence header (default 0)");
  Say("Program flags:");
  Say("  -list <file>          one task per line (the task flags above; # starts a comment line)");
  Say("  -hw                   the first hardware adapter instead of WARP");
  Say("  -v                    print the decoder's log lines (MEMORY ...) and the player-style task lines");
  Say("  -debuglayer           switch the D3D12 debug layer on (needs the Graphics Tools feature of Windows)");
  Say("                        and print its messages after every task; -gpuvalidation adds GPU-based validation");
  Say("  -sm6test              run the Shader Model 6 test shaders on the device before the tasks, one 'SM6TEST ...'");
  Say("                        line per shader (docs/test-player.md); needs no stream: without -i and -list the");
  Say("                        program ends after the test");
  Say("  -playersmoke <dir>    run the decoder code of the test player (testing/player_common) through rav1g.h on");
  Say("                        the tasks of <dir>/args/args_av1.txt, as the player does in its LocalState; its log");
  Say("                        goes to stdout. Needs no -i. Exit code 0: the player ended its launch, 2: it hung");
  Say("");
  Say("Exit code: 0 all tasks decoded and matched, 1 a frame or a frame count differs, 2 a decode error (or");
  Say("-sm6test could not run its test), 3 bad arguments or no device, 4 timeout. One 'RESULT ...' line per task");
  Say("on stdout.");
}

static bool ParseInt(const std::string& s, int* v) {
  if (s.empty()) return false;
  char* end = NULL;
  const long x = strtol(s.c_str(), &end, 10);
  if (!end || *end) return false;
  *v = (int)x;
  return true;
}

// Applies the arguments to the task (and to the globals when given). Returns false with a message.
static bool ParseArgs(const std::vector<std::string>& args, Task* task, Globals* globals, std::string* err) {
  for (size_t i = 0; i < args.size(); ++i) {
    const std::string& a = args[i];
    auto value = [&](std::string* dst) -> bool {
      if (i + 1 >= args.size()) {
        *err = a + " needs a value";
        return false;
      }
      *dst = args[++i];
      return true;
    };
    auto number = [&](int* dst) -> bool {
      std::string s;
      if (!value(&s)) return false;
      if (!ParseInt(s, dst)) {
        *err = a + " needs a number, not '" + s + "'";
        return false;
      }
      return true;
    };
    if (a == "-i") {
      if (!value(&task->input)) return false;
    } else if (a == "-name") {
      if (!value(&task->name)) return false;
    } else if (a == "-check") {
      if (!value(&task->check)) return false;
    } else if (a == "-md5_frame_check") {
      task->md5_frame_check = true;
    } else if (a == "-save_md5") {
      if (!value(&task->save_md5)) return false;
    } else if (a == "-aom_md5") {
      task->aom_md5 = true;
    } else if (a == "-o") {
      if (!value(&task->out_yuv)) return false;
    } else if (a == "-save_wrong_yuv") {
      if (!value(&task->save_wrong_yuv)) return false;
    } else if (a == "-wrong_limit") {
      if (!number(&task->wrong_limit)) return false;
    } else if (a == "-dumpstages") {
      if (!value(&task->dumpstages)) return false;
    } else if (a == "-dumpdir") {
      if (!value(&task->dumpdir)) return false;
    } else if (a == "-limit") {
      if (!number(&task->limit)) return false;
    } else if (a == "-t") {
      if (!number(&task->threads)) return false;
    } else if (a == "-queue_size") {
      if (!number(&task->queue_size)) return false;
    } else if (a == "-timeout") {
      if (!number(&task->timeout_s)) return false;
    } else if (a == "-slots") {
      if (!number(&task->slots)) return false;
    } else if (a == "-lowmemparts") {
      if (!number(&task->lowmem_parts)) return false;
    } else if (a == "-lowmem") {
      task->lowmem = true;
    } else if (a == "-cdefv1") {
      task->cdefv1 = true;
    } else if (a == "-cdefv2") {
      task->cdefv2 = true;
    } else if (a == "-nofitsize") {
      task->nofitsize = true;
    } else if (a == "-fitsize") {
      task->fitsize = true;
    } else if (a == "-intrav2") {
      task->intrav2 = true;
    } else if (a == "-intraopt") {
      task->intraopt = true;
    } else if (a == "-asyncclear") {
      task->asyncclear = true;
    } else if (a == "-lfhsplit") {
      if (!number(&task->lfhsplit)) return false;
    } else if (a == "-intraedgefix") {
      task->intraedgefix = true;
    } else if (a == "-poisonmi") {
      task->poison = AV1_POISON_MODE_INFO;
    } else if (a == "-poisonbuf") {
      task->poison = AV1_POISON_BUFFERS;
    } else if (a == "-interfast") {
      task->interfast = AV1_INTER_FAST_ON;
    } else if (a == "-interfastlists") {
      task->interfast = AV1_INTER_FAST_LISTS_ONLY;
    } else if (a == "-interfastmode") {
      if (!number(&task->interfast)) return false;
    } else if (a == "-gputime") {
      task->gputime = true;
    } else if (a == "-gputimedetail") {
      task->gputime = true;
      if (task->gputime_detail < 1) task->gputime_detail = 1;
    } else if (a == "-gputimeisolate") {
      task->gputime = true;
      task->gputime_detail = 2;
    } else if (a == "-progress") {
      task->progress = true;
    } else if (a == "-alllayers") {
      task->all_layers = true;
    } else if (a == "-oppoint") {
      if (!number(&task->oppoint)) return false;
    } else if (globals && a == "-hw") {
      globals->hardware = true;
    } else if (globals && a == "-v") {
      globals->verbose = true;
    } else if (globals && a == "-debuglayer") {
      globals->debug_layer = true;
    } else if (globals && a == "-gpuvalidation") {
      globals->debug_layer = true;
      globals->gpu_validation = true;
    } else if (globals && a == "-sm6test") {
      globals->sm6test = true;
    } else if (globals && a == "-playersmoke") {
      if (!value(&globals->player_smoke)) return false;
    } else if (globals && a == "-list") {
      if (!value(&globals->list)) return false;
    } else {
      *err = "unknown argument '" + a + "'";
      return false;
    }
  }
  return true;
}

// The checks the player's parser makes after reading a task line.
static bool CheckTask(Task* task, std::string* err) {
  char ss[160];
  if (task->input.empty()) {
    *err = "no stream (-i)";
    return false;
  }
  if (task->cdefv1 && task->cdefv2) {
    *err = "-cdefv1 and -cdefv2 cannot be used together";
    return false;
  }
  if (task->nofitsize && task->fitsize) {
    *err = "-nofitsize and -fitsize cannot be used together";
    return false;
  }
  if (task->slots < WD_SLOTS_MIN || task->slots > WD_SLOTS_MAX) {
    snprintf(ss, sizeof(ss), "-slots %d is not allowed: the decoder takes %d to %d slots", task->slots, WD_SLOTS_MIN,
             WD_SLOTS_MAX);
    *err = ss;
    return false;
  }
  if (task->lowmem_parts < 0 || task->lowmem_parts > WD_LOWMEM_ALL) {
    snprintf(ss, sizeof(ss), "-lowmemparts %d is not allowed: the sum of 1, 2 and 4, at most %d", task->lowmem_parts,
             WD_LOWMEM_ALL);
    *err = ss;
    return false;
  }
  if (task->lowmem) task->lowmem_parts = WD_LOWMEM_ALL;
  if (task->lfhsplit < 0 || task->lfhsplit > 2) {
    snprintf(ss, sizeof(ss), "-lfhsplit %d is not allowed: 0, 1 or 2", task->lfhsplit);
    *err = ss;
    return false;
  }
  if (task->queue_size < 1 || task->queue_size > WD_QUEUE_SIZE_MAX) {
    snprintf(ss, sizeof(ss), "-queue_size %d is not allowed: 1 to %d", task->queue_size, WD_QUEUE_SIZE_MAX);
    *err = ss;
    return false;
  }
  if (task->threads < 1) task->threads = 1;
  if (!task->dumpstages.empty() && !ParseStageDumpFrames(task->dumpstages, &task->dump_frames)) {
    *err = "-dumpstages " + task->dumpstages + " is not a frame list like 0,3,10-12 of at most 256 frames";
    return false;
  }
  if (task->md5_frame_check && task->check.empty()) task->check = task->input + ".md5";
  if (task->name.empty()) {
    const size_t pos = task->input.find_last_of("\\/");
    task->name = pos == std::string::npos ? task->input : task->input.substr(pos + 1);
  }
  // The name is one token of the result line.
  for (size_t i = 0; i < task->name.size(); ++i) {
    const char c = task->name[i];
    if (c == ' ' || c == '\t' || c == '"') task->name[i] = '_';
  }
  return true;
}

// A line of a list file into arguments: split at white space, double quotes keep a value together.
static std::vector<std::string> SplitLine(const std::string& line) {
  std::vector<std::string> out;
  std::string cur;
  bool quoted = false;
  bool have = false;
  for (size_t i = 0; i < line.size(); ++i) {
    const char c = line[i];
    if (c == '"') {
      quoted = !quoted;
      have = true;
    } else if (!quoted && (c == ' ' || c == '\t' || c == '\r' || c == '\n')) {
      if (have) out.push_back(cur);
      cur.clear();
      have = false;
    } else {
      cur += c;
      have = true;
    }
  }
  if (have) out.push_back(cur);
  return out;
}

static std::string Narrow(const wchar_t* w) {
  std::string s;
  for (; w && *w; ++w) s += (*w >= 32 && *w < 127) ? (char)*w : '?';
  return s;
}

static std::string Hex32(HRESULT hr) {
  char ss[16];
  snprintf(ss, sizeof(ss), "0x%08X", (unsigned)hr);
  return ss;
}

static std::string DigestText(const unsigned char digest[16]) {
  char ss[33];
  for (int i = 0; i < 16; ++i) snprintf(ss + 2 * i, 3, "%02x", digest[i]);
  return std::string(ss, 32);
}

// The pool of output images the decoder writes its frames into: readback buffers, mapped for as long as
// they exist. The decoder takes one for every frame that is output (on its decode thread) and hands it back
// with the frame-ready callback (on its GPU completion thread), where the frame is hashed and the buffer
// goes back into the pool; this is the player's -nopresent path with readback buffers only.
class OutputPool {
 public:
  OutputPool(ID3D12Device* device, int count) : device_(device), buffers_(count) {}
  ~OutputPool() {
    for (size_t i = 0; i < buffers_.size(); ++i) Drop(&buffers_[i]);
  }

  struct Buffer {
    ComPtr<ID3D12Resource> resource;
    void* mem = NULL;
    size_t size = 0;
    bool rented = false;
  };

  // Waits for a free buffer of at least `size` bytes. NULL when none came back in time or the device
  // refused the buffer; `why` then says which.
  Buffer* Rent(size_t size, std::string* why) {
    std::unique_lock<std::mutex> lock(cs_);
    Buffer* buffer = NULL;
    const bool got = cv_.wait_for(lock, std::chrono::seconds(kRentWaitSeconds), [&] {
      for (size_t i = 0; i < buffers_.size(); ++i) {
        if (!buffers_[i].rented) {
          buffer = &buffers_[i];
          return true;
        }
      }
      return false;
    });
    if (!got) {
      char ss[128];
      snprintf(ss, sizeof(ss), "no output image came back to the pool of %d within %d s", (int)buffers_.size(),
               kRentWaitSeconds);
      *why = ss;
      return NULL;
    }
    if (buffer->size < size) {
      Drop(buffer);
      D3D12_HEAP_PROPERTIES heap = {};
      heap.Type = D3D12_HEAP_TYPE_READBACK;
      D3D12_RESOURCE_DESC desc = {};
      desc.Dimension = D3D12_RESOURCE_DIMENSION_BUFFER;
      desc.Width = size;
      desc.Height = 1;
      desc.DepthOrArraySize = 1;
      desc.MipLevels = 1;
      desc.Format = DXGI_FORMAT_UNKNOWN;
      desc.SampleDesc.Count = 1;
      desc.Layout = D3D12_TEXTURE_LAYOUT_ROW_MAJOR;
      HRESULT hr = device_->CreateCommittedResource(&heap, D3D12_HEAP_FLAG_NONE, &desc, D3D12_RESOURCE_STATE_COPY_DEST,
                                                    NULL, IID_PPV_ARGS(&buffer->resource));
      if (SUCCEEDED(hr)) hr = buffer->resource->Map(0, NULL, &buffer->mem);
      if (FAILED(hr)) {
        Drop(buffer);
        *why = "readback buffer of " + std::to_string(size) + " bytes: hr " + Hex32(hr);
        return NULL;
      }
      buffer->size = size;
    }
    buffer->rented = true;
    return buffer;
  }

  void Return(void* priv) {
    std::lock_guard<std::mutex> lock(cs_);
    for (size_t i = 0; i < buffers_.size(); ++i) {
      if (&buffers_[i] == priv) {
        buffers_[i].rented = false;
        cv_.notify_all();
        return;
      }
    }
  }

  int Rented() {
    std::lock_guard<std::mutex> lock(cs_);
    int n = 0;
    for (size_t i = 0; i < buffers_.size(); ++i) n += buffers_[i].rented ? 1 : 0;
    return n;
  }

 private:
  enum { kRentWaitSeconds = 600 };
  static void Drop(Buffer* buffer) {
    if (buffer->resource && buffer->mem) buffer->resource->Unmap(0, NULL);
    buffer->resource.Reset();
    buffer->mem = NULL;
    buffer->size = 0;
  }
  ComPtr<ID3D12Device> device_;
  std::vector<Buffer> buffers_;
  std::mutex cs_;
  std::condition_variable cv_;
};

// What a task collects, from the decoder's threads.
struct Run {
  const Task* task = NULL;
  OutputPool* pool = NULL;
  std::mutex cs;
  std::vector<std::string> md5;     // one per output frame, in output order
  std::vector<std::string> ref;     // the reference, when there is one
  bool have_ref = false;
  int matched = 0;
  int first_bad = -1;
  int mismatch_lines = 0;
  int wrong_saved = 0;
  unsigned int width = 0;           // of the first output frame
  unsigned int height = 0;
  unsigned int bit_depth = 0;
  int monochrome = 0;
  int size_changes = 0;
  bool end_seen = false;            // the decoder said that no more frames come
  // -gputime: the records of the decoder's completion thread, as the player keeps them.
  perfstats::PerfStats perf;
  std::mutex perf_cs;
  std::chrono::steady_clock::time_point perf_start;
  std::string pool_error;
  FILE* out_file = NULL;
  std::string io_error;
};

// The visible samples of a frame, in the order they are hashed: luma rows, then U, then V; one byte a sample
// for 8-bit, two bytes (low byte first) above that. A monochrome frame is luma only, or, with -aom_md5, luma
// and two chroma planes of the mid grey value, which is what aomdec writes for --i420 and so what AOM's .md5
// files of monochrome streams hash.
template <class Sink>
static void ForEachRow(const aom_image_t* img, bool aom_md5, Sink sink) {
  const unsigned int bytes = img->bit_depth == 8 ? 1 : 2;
  const unsigned int w_c = (img->d_w + 1) / 2;
  const unsigned int h_c = (img->d_h + 1) / 2;
  const unsigned char* ptr = img->planes[0];
  for (unsigned int y = 0; y < img->d_h; ++y) {
    sink(ptr, img->d_w * bytes);
    ptr += img->stride[0];
  }
  if (!img->monochrome) {
    for (int plane = 1; plane < 3; ++plane) {
      ptr = img->planes[plane];
      for (unsigned int y = 0; y < h_c; ++y) {
        sink(ptr, w_c * bytes);
        ptr += img->stride[plane];
      }
    }
  } else if (aom_md5) {
    std::vector<unsigned char> grey(w_c * bytes);
    const unsigned int value = 1u << (img->bit_depth - 1);
    for (unsigned int x = 0; x < w_c; ++x) {
      if (bytes == 1) {
        grey[x] = (unsigned char)value;
      } else {
        grey[2 * x] = (unsigned char)(value & 0xff);
        grey[2 * x + 1] = (unsigned char)(value >> 8);
      }
    }
    for (int plane = 1; plane < 3; ++plane)
      for (unsigned int y = 0; y < h_c; ++y) sink(grey.data(), w_c * bytes);
  }
}

static void WriteFrame(const aom_image_t* img, bool aom_md5, FILE* file, std::string* error) {
  ForEachRow(img, aom_md5, [&](const unsigned char* row, unsigned int size) {
    if (error->empty() && fwrite(row, 1, size, file) != size) *error = "write error";
  });
}

static int GetBuffer(void* priv, size_t buf_size, uint16_t w, uint16_t h, frame_buffer_type fb_type,
                     av1_decoded_frame_buffer_t* fb) {
  (void)w;
  (void)h;
  (void)fb_type;
  Run* run = static_cast<Run*>(priv);
  if (!run) return -1;
  std::string why;
  OutputPool::Buffer* buffer = run->pool->Rent(buf_size, &why);
  if (!buffer) {
    std::lock_guard<std::mutex> lock(run->cs);
    if (run->pool_error.empty()) run->pool_error = why;
    return -1;
  }
  fb->dx12_buffer = buffer->resource.Get();
  fb->buffer_host_ptr = buffer->mem;
  fb->buffer_size = buffer->size;
  fb->dx12_texture[0] = NULL;
  fb->dx12_texture[1] = NULL;
  fb->dx12_texture[2] = NULL;
  fb->priv = buffer;
  return 0;
}

static int ReleaseBuffer(void* priv, void* buffer_priv) {
  Run* run = static_cast<Run*>(priv);
  if (!run) return -1;
  run->pool->Return(buffer_priv);
  return 0;
}

// -gputime: one record per command list of the decoder, from its GPU completion thread.
static void PerfFrame(void* priv, const av1_perf_frame* frame) {
  Run* run = static_cast<Run*>(priv);
  if (!run || !frame) return;
  std::lock_guard<std::mutex> lock(run->perf_cs);
  run->perf.AddFrame(*frame);
}

// The decoder's frame-ready callback: a frame whose GPU work is complete, or NULL after the last one.
static int NotifyFrameReady(void* priv, void* image) {
  Run* run = static_cast<Run*>(priv);
  if (!run) return -1;
  const aom_image_t* img = static_cast<const aom_image_t*>(image);
  if (!img) {
    std::lock_guard<std::mutex> lock(run->cs);
    run->end_seen = true;
    return 0;
  }
  const Task& task = *run->task;
  if (task.gputime) {
    const double now_ms =
        std::chrono::duration<double, std::milli>(std::chrono::steady_clock::now() - run->perf_start).count();
    std::lock_guard<std::mutex> lock(run->perf_cs);
    run->perf.AddOutput(now_ms);
  }
  MD5Context ctx;
  unsigned char digest[16];
  MD5Init(&ctx);
  ForEachRow(img, task.aom_md5,
             [&](const unsigned char* row, unsigned int size) { MD5Update(&ctx, row, size); });
  MD5Final(digest, &ctx);
  const std::string md5 = DigestText(digest);
  {
    std::lock_guard<std::mutex> lock(run->cs);
    const int n = (int)run->md5.size();
    run->md5.push_back(md5);
    if (n == 0) {
      run->width = img->d_w;
      run->height = img->d_h;
      run->bit_depth = img->bit_depth;
      run->monochrome = img->monochrome;
    } else if (img->d_w != run->width || img->d_h != run->height) {
      // Counted against the size of the first frame; the md5 of a frame is always over its own size.
      ++run->size_changes;
    }
    bool wrong = false;
    if (run->have_ref) {
      if (n < (int)run->ref.size() && run->ref[n] == md5) {
        ++run->matched;
      } else {
        wrong = true;
        if (run->first_bad < 0) run->first_bad = n;
        if (run->mismatch_lines < 16) {
          Say("MISMATCH frame %d (%ux%u %u-bit%s): expected %s, got %s", n, img->d_w, img->d_h, img->bit_depth,
              img->monochrome ? " monochrome" : "",
              n < (int)run->ref.size() ? run->ref[n].c_str() : "no frame (the reference has ended)", md5.c_str());
        } else if (run->mismatch_lines == 16) {
          Say("MISMATCH ... further frames are not listed");
        }
        ++run->mismatch_lines;
      }
    }
    if (run->out_file && run->io_error.empty()) WriteFrame(img, task.aom_md5, run->out_file, &run->io_error);
    if (wrong && !task.save_wrong_yuv.empty() && (task.wrong_limit < 0 || run->wrong_saved < task.wrong_limit)) {
      ++run->wrong_saved;
      const std::string path = task.save_wrong_yuv + "/" + task.name + "_" + std::to_string(n) + ".yuv";
      FILE* f = fopen(path.c_str(), "wb");
      std::string error;
      if (f) {
        WriteFrame(img, task.aom_md5, f, &error);
        fclose(f);
      }
      if (!f || !error.empty()) Say("cannot write %s", path.c_str());
    }
  }
  run->pool->Return(img->fb2_priv);
  return 0;
}

// Reads a reference md5 file: the first 32 characters of every line that starts with 32 hex digits.
static bool ReadReference(const std::string& path, std::vector<std::string>* out) {
  std::ifstream input(path.c_str());
  if (!input.is_open()) return false;
  std::string line;
  while (std::getline(input, line)) {
    if (line.size() < 32) continue;
    bool hex = true;
    for (int i = 0; i < 32 && hex; ++i) {
      const char c = line[i];
      hex = (c >= '0' && c <= '9') || (c >= 'a' && c <= 'f') || (c >= 'A' && c <= 'F');
    }
    if (!hex) continue;
    std::string md5 = line.substr(0, 32);
    for (int i = 0; i < 32; ++i)
      if (md5[i] >= 'A' && md5[i] <= 'F') md5[i] = (char)(md5[i] - 'A' + 'a');
    out->push_back(md5);
  }
  return true;
}

static bool g_verbose = false;

static void DecoderLogLine(void* priv, int is_error, const char* line) {
  (void)priv;
  if (is_error || g_verbose) Say("%s%s", is_error ? "DECODER ERROR: " : "decoder: ", line);
}

// The task that is running, for the watchdog and the crash filter: what they print must name it.
static std::mutex g_current_cs;
static std::string g_current_name;
static Run* g_current_run = NULL;
static std::chrono::steady_clock::time_point g_current_start;
static int g_current_timeout_s = 0;
static HANDLE g_watchdog_stop = NULL;

// One line with the memory of the process and of the machine: the tasks of a list run in one process, and a
// D3D12 device that runs out of memory is removed.
static void MemoryLine(const char* when) {
  PROCESS_MEMORY_COUNTERS_EX pmc = {};
  pmc.cb = sizeof(pmc);
  MEMORYSTATUSEX status = {};
  status.dwLength = sizeof(status);
  if (!GetProcessMemoryInfo(GetCurrentProcess(), (PROCESS_MEMORY_COUNTERS*)&pmc, sizeof(pmc)) ||
      !GlobalMemoryStatusEx(&status)) {
    return;
  }
  const double mib = 1024.0 * 1024.0;
  Say("PROCESS %s: private %.0f MiB (peak page file use %.0f MiB), working set %.0f MiB; machine: %.0f MiB of "
      "%.0f MiB physical memory free, %.0f MiB of %.0f MiB commit limit free",
      when, pmc.PrivateUsage / mib, pmc.PeakPagefileUsage / mib, pmc.WorkingSetSize / mib, status.ullAvailPhys / mib,
      status.ullTotalPhys / mib, status.ullAvailPageFile / mib, status.ullTotalPageFile / mib);
}

static void ResultLine(const std::string& name, const char* status, int frames, int matched, int first_bad,
                       int ref_frames, double decode_ms, double create_ms, unsigned int width, unsigned int height,
                       unsigned int bit_depth, const std::string& detail) {
  std::string clean = detail;
  for (size_t i = 0; i < clean.size(); ++i)
    if (clean[i] == '"' || clean[i] == '\r' || clean[i] == '\n') clean[i] = '\'';
  Say("RESULT stream=%s status=%s frames=%d matched=%d first_bad=%d ref_frames=%d decode_ms=%.0f create_ms=%.0f "
      "size=%ux%u bitdepth=%u detail=\"%s\"",
      name.c_str(), status, frames, matched, first_bad, ref_frames, decode_ms, create_ms, width, height, bit_depth,
      clean.c_str());
}

// -timeout: a task that runs too long gets its result line from here, and the process ends. The frames
// that came out up to then are counted without the task's lock (the task may hold it); the lock on the
// current task is kept to the end, so the task cannot go away under this thread.
static DWORD WINAPI WatchdogThread(LPVOID) {
  for (;;) {
    if (WaitForSingleObject(g_watchdog_stop, 500) != WAIT_TIMEOUT) return 0;
    std::lock_guard<std::mutex> lock(g_current_cs);
    if (!g_current_run || g_current_timeout_s <= 0) continue;
    const double elapsed = std::chrono::duration<double>(std::chrono::steady_clock::now() - g_current_start).count();
    if (elapsed < (double)g_current_timeout_s) continue;
    Run* run = g_current_run;
    char detail[160];
    snprintf(detail, sizeof(detail), "no result after %d s; frames out so far are counted, %d output images rented",
             g_current_timeout_s, run->pool ? run->pool->Rented() : 0);
    const int frames = (int)run->md5.size();
    ResultLine(g_current_name, "timeout", frames, run->matched, run->first_bad,
               run->have_ref ? (int)run->ref.size() : 0, elapsed * 1000.0, 0.0, run->width, run->height,
               run->bit_depth, detail);
    fflush(stdout);
    TerminateProcess(GetCurrentProcess(), (UINT)kExitTimeout);
    return 0;
  }
}

// An access violation or the like inside the decoder: say where, and end with the exception code as the
// exit code instead of a dialog.
static LONG WINAPI CrashFilter(EXCEPTION_POINTERS* info) {
  const DWORD code = info && info->ExceptionRecord ? info->ExceptionRecord->ExceptionCode : 0;
  void* address = info && info->ExceptionRecord ? info->ExceptionRecord->ExceptionAddress : NULL;
  char module_name[MAX_PATH] = "?";
  uintptr_t offset = 0;
  HMODULE module = NULL;
  if (address && GetModuleHandleExA(GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS | GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
                                    (LPCSTR)address, &module) && module) {
    GetModuleFileNameA(module, module_name, sizeof(module_name));
    offset = (uintptr_t)address - (uintptr_t)module;
  }
  const char* base = strrchr(module_name, '\\');
  printf("CRASH exception 0x%08lX at %s+0x%llX (thread %lu)\n", (unsigned long)code, base ? base + 1 : module_name,
         (unsigned long long)offset, (unsigned long)GetCurrentThreadId());
  fflush(stdout);
  TerminateProcess(GetCurrentProcess(), code ? code : 0xE0000001);
  return EXCEPTION_EXECUTE_HANDLER;
}

struct Device {
  ComPtr<ID3D12Device> device;
  ComPtr<ID3D12CommandQueue> queue;
  void* psos = NULL;
};

// -debuglayer: what the debug layer has stored since the last call, at most 60 messages.
static void PrintDebugMessages(ID3D12Device* device) {
  ComPtr<ID3D12InfoQueue> queue;
  if (FAILED(device->QueryInterface(IID_PPV_ARGS(&queue)))) return;
  const UINT64 count = queue->GetNumStoredMessages();
  const UINT64 dropped = queue->GetNumMessagesDiscardedByMessageCountLimit();
  Say("DEBUGLAYER %llu messages stored, %llu discarded by the limit", (unsigned long long)count,
      (unsigned long long)dropped);
  for (UINT64 i = 0; i < count && i < 60; ++i) {
    SIZE_T size = 0;
    queue->GetMessage(i, NULL, &size);
    if (!size) continue;
    std::vector<unsigned char> storage(size);
    D3D12_MESSAGE* message = reinterpret_cast<D3D12_MESSAGE*>(storage.data());
    if (FAILED(queue->GetMessage(i, message, &size))) continue;
    Say("DEBUGLAYER [severity %d, id %d] %s", (int)message->Severity, (int)message->ID,
        message->pDescription ? message->pDescription : "");
  }
  queue->ClearStoredMessages();
}

static bool CreateDevice(const Globals& globals, Device* out) {
  if (globals.debug_layer) {
    ComPtr<ID3D12Debug> debug;
    const HRESULT debug_hr = D3D12GetDebugInterface(IID_PPV_ARGS(&debug));
    if (SUCCEEDED(debug_hr)) {
      debug->EnableDebugLayer();
      ComPtr<ID3D12Debug1> debug1;
      if (globals.gpu_validation && SUCCEEDED(debug.As(&debug1))) debug1->SetEnableGPUBasedValidation(TRUE);
      Say("DEBUGLAYER on%s", globals.gpu_validation ? ", with GPU-based validation" : "");
    } else {
      Say("DEBUGLAYER not available (hr %s): the Graphics Tools feature of Windows is not installed",
          Hex32(debug_hr).c_str());
    }
  }
  ComPtr<IDXGIFactory4> factory;
  HRESULT hr = CreateDXGIFactory2(0, IID_PPV_ARGS(&factory));
  if (FAILED(hr)) {
    Say("ERROR CreateDXGIFactory2: hr %s", Hex32(hr).c_str());
    return false;
  }
  ComPtr<IDXGIAdapter1> adapter;
  if (globals.hardware) {
    hr = E_FAIL;
    ComPtr<IDXGIAdapter1> candidate;
    for (UINT index = 0; factory->EnumAdapters1(index, &candidate) != DXGI_ERROR_NOT_FOUND; ++index) {
      DXGI_ADAPTER_DESC1 desc = {};
      candidate->GetDesc1(&desc);
      if (!(desc.Flags & DXGI_ADAPTER_FLAG_SOFTWARE)) {
        hr = D3D12CreateDevice(candidate.Get(), D3D_FEATURE_LEVEL_11_0, IID_PPV_ARGS(&out->device));
        if (SUCCEEDED(hr)) {
          adapter = candidate;
          break;
        }
      }
      candidate.Reset();
    }
    if (FAILED(hr) || !out->device) {
      Say("ERROR -hw: no hardware adapter with a D3D12 device (hr %s)", Hex32(hr).c_str());
      return false;
    }
  } else {
    hr = factory->EnumWarpAdapter(IID_PPV_ARGS(&adapter));
    if (FAILED(hr)) {
      Say("ERROR EnumWarpAdapter: hr %s", Hex32(hr).c_str());
      return false;
    }
    hr = D3D12CreateDevice(adapter.Get(), D3D_FEATURE_LEVEL_11_0, IID_PPV_ARGS(&out->device));
    if (FAILED(hr)) {
      Say("ERROR D3D12CreateDevice on the WARP adapter: hr %s", Hex32(hr).c_str());
      return false;
    }
  }
  DXGI_ADAPTER_DESC1 desc = {};
  adapter->GetDesc1(&desc);
  D3D12_FEATURE_DATA_D3D12_OPTIONS options = {};
  const HRESULT options_hr = out->device->CheckFeatureSupport(D3D12_FEATURE_D3D12_OPTIONS, &options, sizeof(options));
  Say("ADAPTER %s (%s), vendor 0x%04X device 0x%04X; shader double precision %s", Narrow(desc.Description).c_str(),
      (desc.Flags & DXGI_ADAPTER_FLAG_SOFTWARE) ? "software" : "hardware", desc.VendorId, desc.DeviceId,
      FAILED(options_hr) ? "not known" : options.DoublePrecisionFloatShaderOps ? "reported" : "not reported");
  D3D12_COMMAND_QUEUE_DESC queue_desc = {};
  queue_desc.Flags = D3D12_COMMAND_QUEUE_FLAG_NONE;
  queue_desc.Type = D3D12_COMMAND_LIST_TYPE_DIRECT;
  hr = out->device->CreateCommandQueue(&queue_desc, IID_PPV_ARGS(&out->queue));
  if (FAILED(hr)) {
    Say("ERROR CreateCommandQueue: hr %s", Hex32(hr).c_str());
    return false;
  }
  return true;
}

static double MsSince(std::chrono::steady_clock::time_point start) {
  return std::chrono::duration<double, std::milli>(std::chrono::steady_clock::now() - start).count();
}

// One task, as the player's AV1Decoder::DecodeOne does it. Returns the exit code of the task.
static int RunTask(const Task& task, Device* dev) {
  Say("BEGIN stream=%s", task.name.c_str());
  MemoryLine("before the task");
  Run run;
  run.task = &task;
  {
    std::lock_guard<std::mutex> lock(g_current_cs);
    g_current_name = task.name;
    g_current_run = &run;
    g_current_start = std::chrono::steady_clock::now();
    g_current_timeout_s = task.timeout_s;
  }
  struct ClearCurrent {
    ~ClearCurrent() {
      std::lock_guard<std::mutex> lock(g_current_cs);
      g_current_run = NULL;
    }
  } clear_current;

  double create_ms = 0.0;
  double decode_ms = 0.0;
  auto fail = [&](const std::string& detail) -> int {
    std::lock_guard<std::mutex> lock(run.cs);
    ResultLine(task.name, "error", (int)run.md5.size(), run.matched, run.first_bad,
               run.have_ref ? (int)run.ref.size() : 0, decode_ms, create_ms, run.width, run.height, run.bit_depth,
               detail);
    return kExitError;
  };

  if (!task.check.empty()) {
    if (!ReadReference(task.check, &run.ref)) return fail("cannot read the reference " + task.check);
    if (run.ref.empty()) return fail("no md5 lines in the reference " + task.check);
    run.have_ref = true;
  }

  StreamReader reader(srmodeSync);
  std::string path(task.input);
  if (reader.Start(path)) return fail("cannot open the stream, or its container is not IVF, WebM, OBU or MP4");

  // The first key frame is looked for before the decoder exists: its sequence header says how large the
  // frames of the stream are and how many bits a sample has.
  ReadBufferWrapper buffer;
  aom_codec_stream_info_t si;
  memset(&si, 0, sizeof(si));
  int skipped = 0;
  for (;;) {
    buffer = reader.GetData();
    if (!buffer) return fail("end of the stream before a key frame");
    if (aom_codec_peek_stream_info(aom_codec_av1_dx(), buffer->Data(), buffer->Size(), &si) != AOM_CODEC_OK)
      return fail("aom_codec_peek_stream_info failed on temporal unit " + std::to_string(skipped));
    if (si.is_kf) break;
    ++skipped;
  }

  // The size the decoder is created for: the stream's largest frame size, rounded up to 128 as the decoder
  // does, and 8-bit buffers for an 8-bit stream; 4096x2176 10-bit with -nofitsize or for a larger stream.
  // A decoder that follows another one of the task (a new sequence that did not fit, below) is at least
  // as large as that one, so a stream that goes back and forth between two sizes gets one decoder.
  unsigned int dec_width = 0;
  unsigned int dec_height = 0;
  unsigned int dec_bitdepth = 0;
  auto size_for_stream = [&](const aom_codec_stream_info_t& info) {
    unsigned int width = WD_DECODER_WIDTH;
    unsigned int height = WD_DECODER_HEIGHT;
    unsigned int bitdepth = WD_DECODER_BITDEPTH;
    if (!task.nofitsize) {
      const unsigned int fit_width = (info.w + 127) & ~127u;
      const unsigned int fit_height = (info.h + 127) & ~127u;
      if (info.w && info.h && fit_width <= WD_DECODER_WIDTH && fit_height <= WD_DECODER_HEIGHT) {
        width = fit_width;
        height = fit_height;
      }
      if (info.bit_depth == 8) bitdepth = 8;
    }
    dec_width = width > dec_width ? width : dec_width;
    dec_height = height > dec_height ? height : dec_height;
    dec_bitdepth = bitdepth > dec_bitdepth ? bitdepth : dec_bitdepth;
  };
  size_for_stream(si);

  OutputPool pool(dev->device.Get(), task.queue_size);
  run.pool = &pool;
  if (!task.out_yuv.empty()) {
    run.out_file = fopen(task.out_yuv.c_str(), "wb");
    if (!run.out_file) return fail("cannot write " + task.out_yuv);
  }
  struct CloseOut {
    Run* run;
    ~CloseOut() {
      if (run->out_file) fclose(run->out_file);
      run->out_file = NULL;
    }
  } close_out = {&run};

  // Creates the decoder for dec_width x dec_height, dec_bitdepth; |info| is the sequence it is for. Empty
  // when the decoder exists, else what failed.
  aom_codec_ctx_t ctx;
  memset(&ctx, 0, sizeof(ctx));
  std::vector<uint8_t> host_mem;
  auto create_decoder = [&](const aom_codec_stream_info_t& info) -> std::string {
    const auto create_start = std::chrono::steady_clock::now();
    aom_codec_dec_cfg_t cfg;
    memset(&cfg, 0, sizeof(cfg));
    cfg.width = dec_width;
    cfg.height = dec_height;
    cfg.bitdepth = dec_bitdepth;
    cfg.threads = task.threads;
    cfg.cfg.ext_partition = 1;
    cfg.allow_lowbitdepth = CONFIG_LOWBITDEPTH;
    cfg.out_buffers_cb.get_out_buffer_cb = GetBuffer;
    cfg.out_buffers_cb.release_out_buffer_cb = ReleaseBuffer;
    cfg.out_buffers_cb.notify_frame_ready_cb = NotifyFrameReady;
    cfg.out_buffers_cb.out_buffers_priv = &run;
    cfg.host_size = 0;
    cfg.tryHDR10x3 = 0;
    cfg.frame_slots = task.slots;
    cfg.intra_v2 = task.intrav2 ? 1 : 0;
    cfg.inter_fast = task.interfast ? 1 : 0;
    cfg.low_memory = task.lowmem_parts ? task.lowmem_parts : AV1_LOW_MEMORY_NONE;
    // No budget guard: a desktop process has no app memory limit to run into.
    cfg.memory_available_cb = NULL;
    cfg.memory_available_priv = NULL;
    av1_codec_query_memory_requirements(&cfg);
    try {
      host_mem.assign(cfg.host_size, 0);
    } catch (const std::bad_alloc&) {
      return "no memory for the host pool of " + std::to_string(cfg.host_size) + " bytes";
    }
    cfg.host_memory = host_mem.data();
    cfg.host_size = host_mem.size();
    cfg.dx12device = dev->device.Get();
    cfg.dx12command_queue = dev->queue.Get();
    cfg.dxPsos = dev->psos;

    Say("TASK %s: stream %ux%u %u-bit, decoder %ux%u %u-bit (%s), slots %d, low memory parts %d, CDEF %s, intra %s%s, "
        "clears %s, threads %d, output pool %d; device buffers %.1f MiB, host pool %.1f MiB",
        task.name.c_str(), info.w, info.h, info.bit_depth, dec_width, dec_height, dec_bitdepth,
        task.nofitsize ? "-nofitsize" : "fit to the stream", task.slots, task.lowmem_parts,
        task.cdefv1 ? "cdef_filter (-cdefv1)" : "cdef_filter_v2", task.intrav2 ? "intra_v2" : "intra_main + intra_filter",
        task.intraopt ? " (intra_main_opt)" : "", task.asyncclear ? "-asyncclear" : "decode thread", task.threads,
        task.queue_size, (double)cfg.device_size / (1024.0 * 1024.0), (double)cfg.host_size / (1024.0 * 1024.0));

    memset(&ctx, 0, sizeof(ctx));
    if (aom_codec_dec_init(&ctx, aom_codec_av1_dx(), &cfg, 0)) {
      const char* message = aom_codec_error(&ctx);
      const char* detail = aom_codec_error_detail(&ctx);
      return std::string("decoder creation failed: ") + (message ? message : "?") +
             ((detail && detail[0]) ? std::string(": ") + detail : std::string());
    }
    // The layers of a stream that has them: one frame per temporal unit unless -alllayers, operating point 0
    // unless -oppoint.
    if (aom_codec_control(&ctx, AV1D_SET_OUTPUT_ALL_LAYERS, task.all_layers ? 1 : 0) ||
        aom_codec_control(&ctx, AV1D_SET_OPERATING_POINT, task.oppoint)) {
      const std::string control_error = std::string("a layer control failed: ") + aom_codec_error(&ctx);
      aom_codec_destroy(&ctx);
      return control_error;
    }
    create_ms += MsSince(create_start);
    return std::string();
  };
  // -poisonmi, -poisonbuf: the decoder fills these buffers when it creates them, so this switch is set before
  // the decoder exists (also the one a new sequence creates later in the task). With -poisonbuf it also
  // fills the constants of a stage whenever it hands them out (ComputeCommandBuffer::Alloc).
  av1_set_poison_buffers(task.poison);
  if (task.poison == AV1_POISON_MODE_INFO)
    Say("POISON %s: the mode info pool (-poisonmi) filled with 0xA5 when the decoder creates it", task.name.c_str());
  else if (task.poison)
    Say("POISON %s: every buffer the CPU writes for the shaders (-poisonbuf) filled with 0xA5 when the decoder "
        "creates it, the constants of a stage again before the stage writes them",
        task.name.c_str());
  {
    const std::string create_error = create_decoder(si);
    if (!create_error.empty()) return fail(create_error);
  }

  // Process-wide switches, set while no frame is in flight, as the player does at the start of a task.
  // -gputime, -gputimedetail, -gputimeisolate: the timing run of the player.
  run.perf_start = std::chrono::steady_clock::now();
  av1_set_perf_callback(task.gputime ? PerfFrame : NULL, &run);
  av1_set_perf_detail(task.gputime ? task.gputime_detail : 0);
  if (task.gputime)
    Say("GPUTIME %s: timing run%s; on WARP the times are those of a CPU rasterizer", task.name.c_str(),
        task.gputime_detail == 2
            ? ", inter breakdown with a UAV barrier and a timestamp behind every dispatch (-gputimeisolate)"
            : task.gputime_detail == 1 ? ", inter breakdown (-gputimedetail)" : "");
  av1_set_cdef_v2(task.cdefv1 ? 0 : 1);
  av1_set_intra_v2(task.intrav2 ? 1 : 0);
  av1_set_intra_opt(task.intraopt ? 1 : 0);
  av1_set_async_clear(task.asyncclear ? 1 : 0);
  av1_set_lf_h_split(task.lfhsplit);
  av1_set_intra_edge_fix(task.intraedgefix ? 1 : 0);
  av1_set_inter_fast(task.interfast);
  if (task.interfast & AV1_INTER_FAST_ON)
    Say("INTERFAST %s: inter blocks by motion vector class, mode %d (one reference: %s; two: %s); frames with a "
        "scaled reference keep the original lists and shaders",
        task.name.c_str(), task.interfast & 7,
        (task.interfast & AV1_INTER_FAST_SINGLE_ORIGINAL) ? "inter_main on every list" : "inter_fast",
        (task.interfast & AV1_INTER_FAST_COMPOUND_ORIGINAL) ? "inter_compound on every list" : "inter_compound_fast");
  if (task.lfhsplit || task.intraedgefix)
    Say("EXPERIMENTS %s:%s%s", task.name.c_str(),
        task.lfhsplit == 1 ? " -lfhsplit 1 (horizontal loop filter: even rows of blocks, then odd)"
                           : task.lfhsplit == 2 ? " -lfhsplit 2 (horizontal loop filter: row of blocks by row)" : "",
        task.intraedgefix ? " -intraedgefix (8-bit above edge filter bound)" : "");
  // -dumpstages: the writer outlives the decoder's completion thread, which the drain below joins.
  StageDumpWriter stage_dumps;
  if (!task.dump_frames.empty()) {
    CreateDirectoryA(task.dumpdir.c_str(), NULL);
    stage_dumps.Start(task.dumpdir, task.name);
    av1_set_stage_dump(StageDumpWriter::Callback, &stage_dumps, task.dump_frames.data(),
                       (int)task.dump_frames.size());
  } else {
    av1_set_stage_dump(NULL, NULL, NULL, 0);
  }

  std::string decode_error;
  int units = 0;
  int limit = task.limit;
  int decoders = 1;
  bool have_decoder = true;
  const auto decode_start = std::chrono::steady_clock::now();
  while (buffer && limit--) {
    const auto call_start = std::chrono::steady_clock::now();
    aom_codec_err_t err = aom_codec_decode(&ctx, buffer->Data(), buffer->Size(), NULL);
    if (task.gputime) {
      std::lock_guard<std::mutex> lock(run.perf_cs);
      run.perf.AddDecodeCall(MsSince(call_start));
    }
    if (err == AOM_CODEC_NEEDS_RECONFIGURE) {
      // A new sequence whose frames are larger, or have more bits, than the decoder was created for
      // (aom/aom_codec.h). Nothing of this temporal unit was decoded. The frames of the sequence before
      // are drained, the decoder is replaced by one for the new sequence, sized from this temporal unit
      // as the first one was from the first, and the temporal unit is passed again.
      const std::string why = aom_codec_error_detail(&ctx) ? aom_codec_error_detail(&ctx) : "";
      aom_codec_stream_info_t new_si;
      memset(&new_si, 0, sizeof(new_si));
      const unsigned int old_width = dec_width, old_height = dec_height, old_bitdepth = dec_bitdepth;
      if (aom_codec_peek_stream_info(aom_codec_av1_dx(), buffer->Data(), buffer->Size(), &new_si) != AOM_CODEC_OK ||
          !new_si.is_kf) {
        decode_error = "temporal unit " + std::to_string(skipped + units) + " needs another decoder (" + why +
                       ") and does not start a sequence with a key frame";
        break;
      }
      size_for_stream(new_si);
      if (dec_width == old_width && dec_height == old_height && dec_bitdepth == old_bitdepth) {
        decode_error = "temporal unit " + std::to_string(skipped + units) + " needs a larger decoder (" + why +
                       ") than this program creates" + (task.nofitsize ? " with -nofitsize" : "");
        break;
      }
      aom_codec_decode(&ctx, NULL, 0, NULL);
      aom_codec_destroy(&ctx);
      // -dumpstages: the drain has handed over the dumps of the sequence before; the frame numbers of the new
      // decoder start at 0 again, and its dumps go to files of their own.
      stage_dumps.NextSequence();
      const std::string create_error = create_decoder(new_si);
      if (!create_error.empty()) {
        have_decoder = false;  // nothing to drain or destroy below
        decode_error = "temporal unit " + std::to_string(skipped + units) + ", new decoder for the sequence of " +
                       std::to_string(new_si.w) + "x" + std::to_string(new_si.h) + ": " + create_error;
        break;
      }
      ++decoders;
      Say("RECONFIGURE %s: temporal unit %d starts a sequence of %ux%u %u-bit frames (%s); new decoder %ux%u %u-bit",
          task.name.c_str(), skipped + units, new_si.w, new_si.h, new_si.bit_depth, why.c_str(), dec_width,
          dec_height, dec_bitdepth);
      err = aom_codec_decode(&ctx, buffer->Data(), buffer->Size(), NULL);
    }
    if (err) {
      const char* message = aom_codec_error(&ctx);
      const char* detail = aom_codec_error_detail(&ctx);
      decode_error = "aom_codec_decode failed on temporal unit " + std::to_string(skipped + units) + ": " +
                     (message ? message : "?") + ((detail && detail[0]) ? std::string(": ") + detail : std::string());
      break;
    }
    ++units;
    buffer = reader.GetData();
  }
  // Drain: joins the decoder's GPU completion thread, so every frame that was submitted has been handed over.
  if (have_decoder) aom_codec_decode(&ctx, NULL, 0, NULL);
  decode_ms = MsSince(decode_start);
  buffer.Reset();
  if (have_decoder) aom_codec_destroy(&ctx);
  av1_set_stage_dump(NULL, NULL, NULL, 0);
  // -gputime: the completion thread is joined, so the records are complete. The summary of the player, and
  // with -progress its per-frame lines.
  av1_set_perf_callback(NULL, NULL);
  av1_set_perf_detail(0);
  if (task.gputime) {
    const perfstats::OutputWaits no_waits = {0, 0.0, 0.0};
    std::lock_guard<std::mutex> lock(run.perf_cs);
    const std::vector<std::string> summary = run.perf.Summary(no_waits);
    for (size_t i = 0; i < summary.size(); ++i) Say("GPUTIME %s", summary[i].c_str());
    if (task.progress) {
      const std::vector<std::string> frames = run.perf.FrameLines();
      for (size_t i = 0; i < frames.size(); ++i) Say("GPUTIME %s", frames[i].c_str());
    }
  }
  std::string dump_error;
  if (!task.dump_frames.empty()) {
    const std::vector<std::string> lines = stage_dumps.TakeLines();
    for (size_t i = 0; i < lines.size(); ++i) Say("%s", lines[i].c_str());
    Say("DUMPSTAGES %s: %d dumps, %.1f MiB, of frames %s in %s", task.name.c_str(), stage_dumps.Dumps(),
        (double)stage_dumps.Bytes() / (1024.0 * 1024.0), task.dumpstages.c_str(), task.dumpdir.c_str());
    dump_error = stage_dumps.Error();
  }

  const HRESULT removed = dev->device->GetDeviceRemovedReason();
  MemoryLine("after the task");
  std::lock_guard<std::mutex> lock(run.cs);
  if (run.out_file) {
    if (fclose(run.out_file)) run.io_error = "write error";
    run.out_file = NULL;
  }
  const int frames = (int)run.md5.size();
  if (!task.save_md5.empty()) {
    // The player's format: "<md5>   <n>" and a line end of CR LF.
    FILE* f = fopen(task.save_md5.c_str(), "wb");
    if (f) {
      for (int i = 0; i < frames; ++i) fprintf(f, "%s   %d\r\n", run.md5[i].c_str(), i);
      if (fclose(f)) f = NULL;
    }
    if (!f && run.io_error.empty()) run.io_error = "cannot write " + task.save_md5;
  }
  if (!dump_error.empty() && run.io_error.empty()) run.io_error = dump_error;
  if (!run.pool_error.empty() && !decode_error.empty()) decode_error += " (" + run.pool_error + ")";
  if (FAILED(removed)) {
    if (!decode_error.empty()) decode_error += "; ";
    decode_error += "the D3D12 device was removed, reason " + Hex32(removed);
  }
  const int ref_frames = run.have_ref ? (int)run.ref.size() : 0;
  char sizes[96];
  snprintf(sizes, sizeof(sizes), "%d temporal units in, %d frames out%s%s", units, frames,
           run.size_changes ? ", frame size changes within the stream" : "",
           decoders > 1 ? ", a new decoder for a larger sequence" : "");
  if (!decode_error.empty()) {
    ResultLine(task.name, "error", frames, run.matched, run.first_bad, ref_frames, decode_ms, create_ms, run.width,
               run.height, run.bit_depth, decode_error + "; " + sizes);
    return kExitError;
  }
  if (!run.io_error.empty()) {
    ResultLine(task.name, "error", frames, run.matched, run.first_bad, ref_frames, decode_ms, create_ms, run.width,
               run.height, run.bit_depth, run.io_error);
    return kExitError;
  }
  if (!run.have_ref) {
    ResultLine(task.name, "nocheck", frames, 0, -1, 0, decode_ms, create_ms, run.width, run.height, run.bit_depth,
               sizes);
    return kExitPass;
  }
  // The frame count is part of the comparison. A task with -limit decodes a part of the stream, so its
  // reference may go on; a stream that is decoded whole must give exactly the frames of the reference.
  std::string detail = sizes;
  bool count_ok = frames <= ref_frames && frames > 0;
  if (task.limit < 0 && frames != ref_frames) count_ok = false;
  if (!count_ok) {
    detail += "; the reference has " + std::to_string(ref_frames) + " frames";
    if (run.first_bad < 0) run.first_bad = frames < ref_frames ? frames : ref_frames;
  }
  const bool pass = count_ok && run.matched == frames;
  ResultLine(task.name, pass ? "pass" : "mismatch", frames, run.matched, run.first_bad, ref_frames, decode_ms,
             create_ms, run.width, run.height, run.bit_depth, detail);
  return pass ? kExitPass : kExitMismatch;
}

int main(int argc, char** argv) {
  SetErrorMode(SEM_FAILCRITICALERRORS | SEM_NOGPFAULTERRORBOX | SEM_NOOPENFILEERRORBOX);
  SetUnhandledExceptionFilter(CrashFilter);
  _set_abort_behavior(0, _WRITE_ABORT_MSG | _CALL_REPORTFAULT);
  setvbuf(stdout, NULL, _IONBF, 0);

  std::vector<std::string> args;
  for (int i = 1; i < argc; ++i) args.push_back(argv[i]);
  if (args.empty()) {
    Usage();
    return kExitUsage;
  }
  Task base;
  Globals globals;
  std::string err;
  if (!ParseArgs(args, &base, &globals, &err)) {
    Say("ERROR %s", err.c_str());
    Usage();
    return kExitUsage;
  }
  g_verbose = globals.verbose;

  std::vector<Task> tasks;
  if (!globals.list.empty()) {
    std::ifstream input(globals.list.c_str());
    if (!input.is_open()) {
      Say("ERROR cannot read the list %s", globals.list.c_str());
      return kExitUsage;
    }
    std::string line;
    int line_no = 0;
    while (std::getline(input, line)) {
      ++line_no;
      const std::vector<std::string> parts = SplitLine(line);
      if (parts.empty() || parts[0][0] == '#') continue;
      Task task = base;
      if (!ParseArgs(parts, &task, NULL, &err) || !CheckTask(&task, &err)) {
        Say("ERROR %s, line %d: %s", globals.list.c_str(), line_no, err.c_str());
        return kExitUsage;
      }
      tasks.push_back(task);
    }
    if (tasks.empty()) {
      Say("ERROR no tasks in the list %s", globals.list.c_str());
      return kExitUsage;
    }
  } else if ((globals.sm6test || !globals.player_smoke.empty()) && base.input.empty()) {
    // -sm6test alone: the test needs a device and no stream. -playersmoke has its tasks in its directory.
  } else {
    if (!CheckTask(&base, &err)) {
      Say("ERROR %s", err.c_str());
      Usage();
      return kExitUsage;
    }
    tasks.push_back(base);
  }

  logging::set_log_priority(globals.verbose ? logging::LOG_INFO : logging::LOG_ERROR);
  av1_set_log_callback(DecoderLogLine, NULL);

  Device dev;
  if (!CreateDevice(globals, &dev)) return kExitUsage;
  // -playersmoke: the player's decoder code does everything from here on itself, as in the player: its
  // log callback, its pipeline states, its decoders.
  if (!globals.player_smoke.empty()) return PlayerSmoke(dev.device.Get(), globals.player_smoke.c_str(), 600);
  // What the device reports about itself ("CAPS ..."), at every launch, and with -sm6test the Shader Model
  // 6 test shaders ("SM6TEST ..."), before anything of the decoder exists (testing/sm6test). A shader the
  // device refuses is a result of the test; the exit code changes only when the test itself broke.
  Sm6LogCaps(dev.device.Get(), SayLine, NULL);
  bool sm6_broken = false;
  if (globals.sm6test) {
    sm6_broken = Sm6RunTests(dev.device.Get(), dev.queue.Get(), SayLine, NULL) != 0;
    if (globals.debug_layer) PrintDebugMessages(dev.device.Get());
    if (tasks.empty()) return sm6_broken ? kExitError : kExitPass;
  }
  const auto pso_start = std::chrono::steady_clock::now();
  dev.psos = av1_create_pipeline_cache_handle(dev.device.Get(), WD_PSO_THREADS);
  if (!dev.psos || av1_wait_pipeline_cache_handle(dev.psos)) {
    Say("ERROR the compute pipeline states could not be created");
    return kExitUsage;
  }
  Say("PIPELINE states created in %.0f ms", MsSince(pso_start));
  Say("SHADERS %s", av1_shader_variant()[0] ? av1_shader_variant() : "normal build");
  if (globals.debug_layer) PrintDebugMessages(dev.device.Get());

  g_watchdog_stop = CreateEventA(NULL, TRUE, FALSE, NULL);
  HANDLE watchdog = CreateThread(NULL, 0, WatchdogThread, NULL, 0, NULL);

  int exit_code = kExitPass;
  for (size_t i = 0; i < tasks.size(); ++i) {
    const int code = RunTask(tasks[i], &dev);
    if (globals.debug_layer) PrintDebugMessages(dev.device.Get());
    if (code == kExitError) {
      // The decoder was destroyed, but a failed decode may leave the device or the process in a state the
      // next task should not inherit: the run ends here and says what was not run.
      exit_code = kExitError;
      if (i + 1 < tasks.size()) Say("STOPPED after a decode error; %d tasks not run", (int)(tasks.size() - i - 1));
      break;
    }
    if (code == kExitMismatch) exit_code = kExitMismatch;
  }

  SetEvent(g_watchdog_stop);
  if (watchdog) {
    WaitForSingleObject(watchdog, INFINITE);
    CloseHandle(watchdog);
  }
  CloseHandle(g_watchdog_stop);
  av1_destroy_pipeline_cache_handle(dev.psos);
  dev.psos = NULL;
  if (sm6_broken && exit_code == kExitPass) exit_code = kExitError;
  return exit_code;
}
