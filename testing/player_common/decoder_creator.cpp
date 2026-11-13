/*
 * Copyright 2020 Google LLC
 *
 */

/*
 * Copyright (c) 2020, Alliance for Open Media. All rights reserved
 *
 * This source code is subject to the terms of the BSD 2 Clause License and
 * the Alliance for Open Media Patent License 1.0. If the BSD 2 Clause License
 * was not distributed with this source code in the LICENSE file, you can
 * obtain it at www.aomedia.org/license/software. If the Alliance for Open
 * Media Patent License 1.0 was not distributed with this source code in the
 * PATENTS file, you can obtain it at www.aomedia.org/license/patent.
 */

#include <stdio.h>
#include <stdlib.h>
#include <vector>
#include <list>
#include <string>
#include <fstream>
#include <sstream>
#include <chrono>
#include <thread>
#include <set>

#include "common\log.h"
#include "cmd_parser.h"
#include "decoder.h"
#include "aom/aom.h"
#include "aom/aom_decoder.h"

#include "common/stream_reader.h"
#include "decoder_creator.h"
#include "sm6test/sm6test.h"

#if (WINAPI_FAMILY == WINAPI_FAMILY_APP)
using namespace Platform;
using namespace Windows::Foundation;
using namespace Windows::Foundation::Collections;
using namespace Windows::Storage::Streams;

using namespace Windows::Storage;
using namespace Platform;
#endif

extern "C" void usage_exit(void) { exit(EXIT_FAILURE); }

#define ERR_DESC(x) \
  { x, #x }
struct error_decript {
  int err;
  const char* err_string;
};
const error_decript error_decripts[] = {ERR_DESC(AOM_CODEC_OK),
                                        ERR_DESC(AOM_CODEC_ERROR),
                                        ERR_DESC(AOM_CODEC_MEM_ERROR),
                                        ERR_DESC(AOM_CODEC_ABI_MISMATCH),
                                        ERR_DESC(AOM_CODEC_INCAPABLE),
                                        ERR_DESC(AOM_CODEC_UNSUP_BITSTREAM),
                                        ERR_DESC(AOM_CODEC_CORRUPT_FRAME),
                                        ERR_DESC(AOM_CODEC_INVALID_PARAM),
                                        ERR_DESC(AOM_CODEC_UNSUP_FEATURE)};
const int errors_size = sizeof(error_decripts) / sizeof(error_decript);

#if (WINAPI_FAMILY == WINAPI_FAMILY_APP)
static std::string pstos(Platform::String ^ ps) {
  std::string s;
  std::wstring ws(ps->Data());
  s.assign(ws.begin(), ws.end());
  return s;
}
#endif

const char* GetVpxErrorString(int err) {
  for (int i = 0; i < errors_size; i++) {
    if (error_decripts[i].err == err) return error_decripts[i].err_string;
  }
  return "Unspecified error";
}

// The log callback of the capability report and the Shader Model 6 test (testing/sm6test).
static void Sm6LogLine(void* priv, const char* line) {
  (void)priv;
  XB_LOGI << line;
}

static void parse_file(const std::string& str, std::vector<one_task>& list) {
  std::ifstream input(str.c_str());
  if (input.is_open()) {
    std::string line;

    while (std::getline(input, line)) {
      if (line[0] == '#') continue;
      std::string ss;
      int brace_is_open = 0;
      list.push_back(one_task(line));
      std::vector<std::string>& current = list.back();
      for (int i = 0; i < line.length(); i++) {
        int s = line[i];
        if (s == '\"') {
          if (brace_is_open) {
            if (ss.length()) current.push_back(ss);
            ss = "";
            brace_is_open = 0;
            continue;
          } else {
            brace_is_open = 1;
            continue;
          }
        }
        if (isspace(s) && !brace_is_open) {
          if (ss.length()) current.push_back(ss);
          ss = "";
          continue;
        }
        ss += (char)s;
      }
      if (ss.length()) current.push_back(ss);
    }
  } else {
    XB_LOGE << "Invalid file path '" << str.c_str() << "'";
  }
}

int Decoders::process_directory(CParameters& params) {
  XB_LOGI << "#######################################";
  XB_LOGI << "# Process directory " << params.GetParams().source_dir_;
  XB_LOGI << "#######################################";
  std::vector<std::string> names;
  std::string search_path = /*read_path_ + "\\" +*/ params.GetParams().source_dir_ + "\\*.*";
  WIN32_FIND_DATAA fd;
  HANDLE hFind = ::FindFirstFileA(search_path.c_str(), &fd);
  if (hFind != INVALID_HANDLE_VALUE) {
    do {
      // read all (real) files in current folder
      // , delete '!' read other 2 default folder . and ..
      if (!(fd.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY)) {
        std::string str(fd.cFileName);
        if (str.substr(str.length() - 4, 4) == ".ivf" || str.substr(str.length() - 4, 4) == ".av1" ||
            str.substr(str.length() - 5, 5) == ".webm" || str.substr(str.length() - 4, 4) == ".obu" ||
            str.substr(str.length() - 4, 4) == ".mp4")
          names.push_back(str);
      }
    } while (::FindNextFileA(hFind, &fd));
    ::FindClose(hFind);
  }
  for (std::vector<std::string>::iterator iter = names.begin(); iter != names.end() && !stop_; iter++) {
    std::string path(params.GetParams().source_dir_);
    if (path.substr(path.length() - 1, 1) != "\\" || path.substr(path.length() - 1, 1) != "/") path += "\\";
    params.GetParamsPtr()->bad_conform_file_ = params.GetParams().bad_conform_dir_ + *iter;
    path += *iter;
    XB_LOGD << "Create decoder for " << params.GetParams().source_file_.c_str();
    params.GetParamsPtr()->source_file_ = path;
    decoder_->Start(&params);
    decoder_->WaitForFinish();
  }
  return 0;
}

Decoders::Decoders(int is_xbox, d3d_resources dx_resources, DisplayCallbacks* display, const char* root_path)
    : is_xbox_(is_xbox), dx_resources_(dx_resources), display_(display), root_path_(root_path ? root_path : "") {}

void Decoders::GetAvailablePaths(std::string& read_path, std::string& write_path, std::string& root_path) {
#if (WINAPI_FAMILY == WINAPI_FAMILY_APP)
  // Packaged sideloads use Device Portal's LocalAppData/LocalState, not loose VS deployment data.
  write_path = read_path = pstos(ApplicationData::Current->LocalFolder->Path);
#else
  read_path = write_path = root_path.empty() ? "./" : root_path;
#endif
}

int Decoders::DecodeLoop() {
  std::string err;
  std::string args_path(_CONFIG_PATH_);
  std::vector<one_task> arg_lists;
  std::string write_path;
  std::string read_path;
  GetAvailablePaths(read_path, write_path, root_path_);
  write_path_ = write_path;
  read_path_ = read_path;
  // What the device reports about itself ("CAPS ..."), once per launch, before anything of the decoder
  // exists (testing/sm6test, docs/test-player.md "Shader Model 6 test").
  Sm6LogCaps(dx_resources_.dx12_device.Get(), Sm6LogLine, NULL);
  std::string full_args_path = read_path + args_path;
  parse_file(full_args_path, arg_lists);
  if (arg_lists.size() == 0) {
    XB_LOGE << "No args were found";
    if (display_) display_->AllJobDoneNotify();
    return -1;
  }
  // -sm6test: not a decoding flag. It may stand on a line of its own or on the line of a stream; the
  // Shader Model 6 test shaders then run once, here, before the pipeline states of the decoder are created
  // ("SM6TEST ..." lines). The flag is taken off the line, and a line that had nothing else is no task: a
  // task file with only -sm6test runs the test and ends.
  bool sm6test = false;
  bool has_tasks = false;
  for (std::vector<one_task>::iterator task = arg_lists.begin(); task != arg_lists.end(); task++) {
    for (std::vector<std::string>::iterator arg = task->begin(); arg != task->end();) {
      if (*arg == "-sm6test") {
        sm6test = true;
        arg = task->erase(arg);
      } else {
        arg++;
      }
    }
    if (!task->empty()) has_tasks = true;
  }
  if (sm6test) {
    AV1Decoder::LogMemory(dx_resources_.dx12_device.Get(), "before the Shader Model 6 test");
    Sm6RunTests(dx_resources_.dx12_device.Get(), dx_resources_.dx12_command_queue.Get(), Sm6LogLine, NULL);
    AV1Decoder::LogMemory(dx_resources_.dx12_device.Get(), "after the Shader Model 6 test");
  }
  if (!has_tasks) {
    XB_LOGI << "No decoding tasks" << (sm6test ? " after -sm6test" : "") << ". Bye";
    if (display_) display_->AllJobDoneNotify();
    return 0;
  }
  // The decoder's own log lines (MEMORY ...) go to the player's log, from the pipeline states on.
  av1_set_log_callback(AV1Decoder::LogLine, NULL);
  AV1Decoder::LogMemory(dx_resources_.dx12_device.Get(), "before the pipeline states");
  dx_resources_.dx12_psos =
      av1_create_pipeline_cache_handle(dx_resources_.dx12_device.Get(), CREATE_SHADERS_THREAD_CNT);
  if (!dx_resources_.dx12_psos) {
    XB_LOGE << "Unable to create compute shaders";
    return -1;
  }
  // The pipeline states are created on threads and the first decoder creation waits for them. Waiting here
  // puts their memory (about 84 MiB of the app's on the console, once per launch) in front of the first
  // "MEMORY app before the decoder" line instead of inside that creation. A state that failed is reported
  // by the decoder creation, as before.
  av1_wait_pipeline_cache_handle(dx_resources_.dx12_psos);
  AV1Decoder::LogMemory(dx_resources_.dx12_device.Get(), "after the pipeline states");
  decoder_ = std::shared_ptr<AV1Decoder>(new AV1Decoder(dx_resources_, display_, &stop_decode_));
  for (std::vector<one_task>::iterator iter = arg_lists.begin(); iter != arg_lists.end() && !stop_; iter++) {
    // check if line is not empty
    if (iter->empty()) continue;
    CParameters params(read_path.c_str(), write_path.c_str());
    int ret = params.Parse(*iter, err);
    if (ret || !err.empty()) {
      XB_LOGE << "Invalid arguments string. " << err;
      continue;
    }
    if (params.GetParams().isDirectory_) {
      if (params.GetParams().conformance_) {
        params.GetParamsPtr()->bad_conform_dir_ = params.GetParamsPtr()->source_dir_ + "\\trg\\";
        params.GetParamsPtr()->source_dir_ += "\\src";
      }
      process_directory(params);
    } else {
      XB_LOGD << "Run decoder for " << params.GetParams().source_file_.c_str();
      decoder_->Start(&params);
      decoder_->WaitForFinish();
      int a = 0;
    }
  }
  // finished all tasks
  decoder_.reset();
  if (display_) display_->AllJobDoneNotify();
  av1_destroy_pipeline_cache_handle(dx_resources_.dx12_psos);
  dx_resources_.dx12_psos = 0;
  return 0;
}
