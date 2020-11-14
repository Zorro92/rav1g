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

#include "pch.h"
#include "playerwnd.h"

static std::string get_log_path(std::string& root_path) {
  char ret[512];
  std::string log_folder(root_path + "\\logs");
  std::wstring log_folder_w(log_folder.begin(), log_folder.end());
  _wmkdir(log_folder_w.c_str());
  struct tm* cur_time;
  __time64_t long_time;
  _time64(&long_time);
  cur_time = _localtime64(&long_time);
  sprintf_s(ret, 512, "%s\\%4d-%2d-%2d_%2d-%2d-%d.txt", log_folder.c_str(), cur_time->tm_year + 1900,
            cur_time->tm_mon + 1, cur_time->tm_mday, cur_time->tm_hour, cur_time->tm_min, cur_time->tm_sec);
  return ret;
}

void PlayerWindow::InitializeLog(logging::LogPriority level) {
  std::string wpath;
  std::string rpath;
  std::string rootpath(_ROOT_FOLDER_);
  Decoders::GetAvailablePaths(rpath, wpath, rootpath);
  logging::set_log_priority(level);
  if (WRITE_LOG_TO_FILE) logging::set_log_file(get_log_path(wpath).c_str());
}

void PlayerWindow::AddSource(AV1Decoder* source) {
  std::unique_lock<std::mutex> lck(cs_);
  decoder_ = source;
}
void PlayerWindow::RemoveSource(AV1Decoder* source) {
  std::unique_lock<std::mutex> lck(cs_);
  decoder_ = 0;
}
void PlayerWindow::AllJobDoneNotify() {
  all_jobs_done_ = 1;
  cv_finish_.notify_one();
}

void PlayerWindow::StopDecode(decodeTask stop_task) {
  if (decoders_) decoders_->Stop(stop_task);
}

int PlayerWindow::Run() {
  pDx12Engine_.reset(new DXEngine(window_.Get()));
  if (pDx12Engine_->OnInit()) return -1;
  d3d_resources dx_resources(pDx12Engine_->Device(), 0);
  decoders_ =
      std::shared_ptr<Decoders>(new Decoders(1, dx_resources, static_cast<DisplayCallbacks*>(this), _ROOT_FOLDER_));
  decoders_->Start();
  return 1;
}

int PlayerWindow::ShowFrame() {
  if (pDx12Engine_) {
    std::unique_lock<std::mutex> lck(cs_);
    pDx12Engine_->OnRender(GetFrame());
  }
  return 0;
}

std::shared_ptr<OneRenderTask> PlayerWindow::GetFrame() {
  if (decoder_) {
    StreamStats stats;
    std::shared_ptr<OutBufferWrapper> frame = decoder_->GetFrame(&stats);
    if (frame) {
      OutBuffer* buffer = frame->Buffer();
      std::shared_ptr<OneRenderTask> task(new OneRenderTask);
      task->render_width = buffer->renderWidth;
      task->render_height = buffer->renderHeight;
      task->textures[0] = buffer->d3d12textures[0];
      task->textures[1] = buffer->d3d12textures[1];
      task->textures[2] = buffer->d3d12textures[2];
      task->shown = buffer->shown;
      buffer->shown = 1;
      task->type = (frame_type)buffer->fb_type;
      wchar_t screen_msg[1024];
      swprintf_s(screen_msg, 1024, L"%s\n%dx%d %dbit%s: %d (%.1f/%d)",
                 std::wstring(stats.fname.begin(), stats.fname.end()).c_str(), buffer->renderWidth,
                 buffer->renderHeight, (buffer->fb_type == fbt8bit) ? 8 : 10,
                 (buffer->fb_type == fbt10x3) ? L"(10x3)" : L"", buffer->frame_no + buffer->frame_no_offset,
                 stats.frate, stats.dropped);
      task->screen_msg = screen_msg;
      return task;
    }
  }
  return 0;
}