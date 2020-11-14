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

#pragma once

#include "decoder.h"

#define CREATE_SHADERS_THREAD_CNT 4  // 0 - sync creation, async (1 - 6 threads num)
#define _CONFIG_PATH_ "/args/args_av1.txt"

typedef enum { TaskNone = 0, taskStop = 1, taskNext = 2, taskStopNoWait = 3, taskStopFlush = 4 } decodeTask;

#if (WINAPI_FAMILY == WINAPI_FAMILY_APP)
using namespace Windows::Storage;
#endif

class Decoders;

interface DisplayCallbacks {
  virtual void AddSource(AV1Decoder * source) = 0;
  virtual void RemoveSource(AV1Decoder * source) = 0;
  virtual void AllJobDoneNotify() = 0;
};

class Decoders {
 public:
  Decoders(int is_xbox, d3d_resources dx_resources, DisplayCallbacks* display, const char* root_path = 0);

  static void GetAvailablePaths(std::string& read_path, std::string& write_path, std::string& root_path);
  void Start() {
    stop_decode_ = 0;
    stop_ = 0;
    work_thread_ = std::thread(RunDecoderLoop, this);
  }

  void Stop(decodeTask stop_task) {
    stop_decode_ = stop_task;
    if (stop_task == taskStop) {
      stop_ = 1;
      WaitForFinish();
    } else if (stop_task == taskStopNoWait) {
      stop_ = 1;
    } else if (stop_task == taskStopFlush) {
      stop_ = 1;
      decoder_->FlushOutput();
      WaitForFinish();
    }
  }

  void WaitForFinish() { work_thread_.join(); }

 protected:
  static void RunDecoderLoop(Decoders* ptr) { ptr->DecodeLoop(); }
  int DecodeLoop();
  int process_directory(CParameters& params);

 private:
  int is_xbox_ = 0;
  DisplayCallbacks* display_;
  std::shared_ptr<AV1Decoder> decoder_;
  std::thread work_thread_;
  volatile int is_running_ = 0;
  volatile int stop_ = 0;
  uint32_t stop_decode_ = 0;
  std::string read_path_;
  std::string write_path_;
  d3d_resources dx_resources_;
  std::string root_path_;
};