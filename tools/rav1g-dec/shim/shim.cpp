/*
 * SPDX-License-Identifier: BSD-2-Clause
 *
 * This source code is subject to the terms of the BSD 2 Clause License and the Alliance for Open Media
 * Patent License 1.0. If the BSD 2 Clause License was not distributed with this source code in the LICENSE
 * file, you can obtain it at www.aomedia.org/license/software. If the Alliance for Open Media Patent
 * License 1.0 was not distributed with this source code in the PATENTS file, you can obtain it at
 * www.aomedia.org/license/patent.
 */

// The C interface of shim.h over the C++ that rav1g-dec shares with the test player.

#include <windows.h>

#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include <mutex>
#include <new>
#include <string>
#include <vector>

#include "common/stream_reader.h"
#include "common/stage_dump.h"
#include "player_common/perf_stats.h"
#include "sm6test/sm6test.h"

#include "shim.h"

// tools_common.c wants this from the program it is linked into (exit code 3: bad arguments).
extern "C" void usage_exit(void) { exit(3); }

// An access violation or the like inside the decoder: say where, and end with the exception code as the
// exit code instead of a dialog (the CrashFilter of warp_decoder, the program of the C++ decoder).
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

void rd_process_init(void) {
  SetErrorMode(SEM_FAILCRITICALERRORS | SEM_NOGPFAULTERRORBOX | SEM_NOOPENFILEERRORBOX);
  SetUnhandledExceptionFilter(CrashFilter);
  _set_abort_behavior(0, _WRITE_ABORT_MSG | _CALL_REPORTFAULT);
  setvbuf(stdout, NULL, _IONBF, 0);
}

void rd_set_log_verbose(int verbose) {
  logging::set_log_priority(verbose ? logging::LOG_INFO : logging::LOG_ERROR);
}

// Stream reader: the synchronous mode of the player's reader, read on the calling thread.
struct rd_reader {
  rd_reader() : reader(srmodeSync) {}
  StreamReader reader;
  ReadBufferWrapper buffer;
};

rd_reader* rd_reader_open(const char* path) {
  rd_reader* r = new (std::nothrow) rd_reader();
  if (!r) return NULL;
  std::string source(path ? path : "");
  if (r->reader.Start(source)) {
    delete r;
    return NULL;
  }
  return r;
}

int rd_reader_next(rd_reader* reader, const uint8_t** data, size_t* size) {
  // The buffer before goes back to the reader before the next one is taken.
  reader->buffer.Reset();
  reader->buffer = reader->reader.GetData();
  if (!reader->buffer) {
    *data = NULL;
    *size = 0;
    return 0;
  }
  *data = reader->buffer->Data();
  *size = reader->buffer->Size();
  return 1;
}

void rd_reader_close(rd_reader* reader) {
  if (!reader) return;
  reader->buffer.Reset();
  delete reader;
}

// -gputime.
struct rd_perf {
  std::mutex cs;
  perfstats::PerfStats stats;
};

rd_perf* rd_perf_new(void) { return new (std::nothrow) rd_perf(); }
void rd_perf_free(rd_perf* perf) { delete perf; }

void rd_perf_add_frame(rd_perf* perf, const av1_perf_frame* frame) {
  if (!perf || !frame) return;
  std::lock_guard<std::mutex> lock(perf->cs);
  perf->stats.AddFrame(*frame);
}

void rd_perf_add_output(rd_perf* perf, double ms) {
  if (!perf) return;
  std::lock_guard<std::mutex> lock(perf->cs);
  perf->stats.AddOutput(ms);
}

void rd_perf_add_decode_call(rd_perf* perf, double ms) {
  if (!perf) return;
  std::lock_guard<std::mutex> lock(perf->cs);
  perf->stats.AddDecodeCall(ms);
}

void rd_perf_lines(rd_perf* perf, int progress, rd_line_fn line, void* ctx) {
  if (!perf) return;
  // No waits in an output queue: the frames are hashed where they are handed over.
  const perfstats::OutputWaits no_waits = {0, 0.0, 0.0};
  std::lock_guard<std::mutex> lock(perf->cs);
  const std::vector<std::string> summary = perf->stats.Summary(no_waits);
  for (size_t i = 0; i < summary.size(); ++i) line(ctx, summary[i].c_str());
  if (progress) {
    const std::vector<std::string> frames = perf->stats.FrameLines();
    for (size_t i = 0; i < frames.size(); ++i) line(ctx, frames[i].c_str());
  }
}

// -dumpstages.
int rd_dump_parse_frames(const char* text, int* frames, int capacity) {
  std::vector<int> list;
  if (!text || !ParseStageDumpFrames(text, &list)) return -1;
  if ((int)list.size() > capacity) return -1;
  for (size_t i = 0; i < list.size(); ++i) frames[i] = list[i];
  return (int)list.size();
}

struct rd_dumps {
  StageDumpWriter writer;
};

rd_dumps* rd_dumps_new(void) { return new (std::nothrow) rd_dumps(); }
void rd_dumps_free(rd_dumps* dumps) { delete dumps; }

void rd_dumps_start(rd_dumps* dumps, const char* dir, const char* name) {
  CreateDirectoryA(dir, NULL);
  dumps->writer.Start(dir, name);
}

void rd_dumps_next_sequence(rd_dumps* dumps) { dumps->writer.NextSequence(); }

void rd_dumps_callback(void* dumps, const av1_stage_dump* dump) {
  StageDumpWriter::Callback(&static_cast<rd_dumps*>(dumps)->writer, dump);
}

void rd_dumps_take_lines(rd_dumps* dumps, rd_line_fn line, void* ctx) {
  const std::vector<std::string> lines = dumps->writer.TakeLines();
  for (size_t i = 0; i < lines.size(); ++i) line(ctx, lines[i].c_str());
}

int rd_dumps_count(rd_dumps* dumps) { return dumps->writer.Dumps(); }
uint64_t rd_dumps_bytes(rd_dumps* dumps) { return dumps->writer.Bytes(); }

int rd_dumps_error(rd_dumps* dumps, rd_line_fn line, void* ctx) {
  const std::string error = dumps->writer.Error();
  if (error.empty()) return 0;
  line(ctx, error.c_str());
  return 1;
}

// The capability report and the Shader Model 6 test.
void rd_sm6_log_caps(void* device, rd_line_fn line, void* ctx) {
  Sm6LogCaps(static_cast<ID3D12Device*>(device), line, ctx);
}

int rd_sm6_run_tests(void* device, void* queue, rd_line_fn line, void* ctx) {
  return Sm6RunTests(static_cast<ID3D12Device*>(device), static_cast<ID3D12CommandQueue*>(queue), line, ctx);
}
