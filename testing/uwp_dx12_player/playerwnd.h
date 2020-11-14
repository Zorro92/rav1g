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

#include "DXEngine.h"
#include <string>
#include <thread>
#include <mutex>
#include "player_common/decoder_creator.h"
#include "player_common/decoder.h"

#define _ROOT_FOLDER_ "G:\\test_data_av1\\av1"
#define WRITE_LOG_TO_FILE 1

class PlayerWindow : public DisplayCallbacks {
 public:
  PlayerWindow(Windows::UI::Core::CoreWindow ^ window) : window_(window) {}
  static void InitializeLog(logging::LogPriority level);
  int ShowFrame();
  int Run();
  void StopDecode(decodeTask stop_task);
  int isJobsDone() { return all_jobs_done_; }

  void WaitForFinishAll() {
    std::unique_lock<std::mutex> lock(cs_finish_);
    cv_finish_.wait(lock, [this] { return all_jobs_done_; });
  }

  // DisplayCallbacks
  virtual void AddSource(AV1Decoder* source) final;
  virtual void RemoveSource(AV1Decoder* source) final;
  virtual void AllJobDoneNotify() final;

 private:
  std::shared_ptr<OneRenderTask> GetFrame();

  BOOL running_ = FALSE;

  std::mutex cs_;
  std::unique_ptr<DXEngine> pDx12Engine_;
  std::shared_ptr<Decoders> decoders_;
  AV1Decoder* decoder_;
  std::mutex cs_finish_;
  std::condition_variable cv_finish_;
  int all_jobs_done_ = 0;

  Platform::Agile<Windows::UI::Core::CoreWindow> window_;
};