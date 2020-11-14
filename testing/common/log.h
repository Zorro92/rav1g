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

#include <iostream>
#include <sstream>
#include <thread>
#include <chrono>
#include <mutex>
#include <string>

//#define XB_LOG_OFF

namespace logging {
typedef enum LogPriority { LOG_DEBUG = 0, LOG_INFO, LOG_WARNING, LOG_ERROR, LOG_FATAL } LogPriority;

class LogMessage {
 public:
  LogMessage(const char* file, int line, LogPriority priority);
  ~LogMessage();
  std::ostream& stream() { return stream_; }

 private:
  void Init(const char* file, int line);
  void RawLog(std::string& message);
  std::ostringstream stream_;
  LogPriority priority_;
  size_t message_start_;
  const char* file_;
  const int line_;
  std::mutex cs_;
};
class DummyLogMessage {
 public:
  DummyLogMessage& operator<<(const char* data) { return *this; }
  DummyLogMessage& operator<<(int data) { return *this; }
  DummyLogMessage& operator<<(size_t data) { return *this; }
  DummyLogMessage& operator<<(void* data) { return *this; }
};
void set_log_priority(logging::LogPriority priority);
void set_log_file(const char* file_path);
}  // namespace logging
#define LOG_MESSAGE_DEBUG ::logging::LogMessage(__FILE__, __LINE__, ::logging::LOG_DEBUG)
#define LOG_MESSAGE_INFO ::logging::LogMessage(__FILE__, __LINE__, ::logging::LOG_INFO)
#define LOG_MESSAGE_WARNING ::logging::LogMessage(__FILE__, __LINE__, ::logging::LOG_WARNING)
#define LOG_MESSAGE_ERROR_ ::logging::LogMessage(__FILE__, __LINE__, ::logging::LOG_ERROR)
#define LOG_MESSAGE_FATAL ::logging::LogMessage(__FILE__, __LINE__, ::logging::LOG_FATAL)

#define LOG_STREAM(severity) LOG_MESSAGE_##severity.stream()
#define DUMMY_LOG ::logging::DummyLogMessage()
#ifndef XB_LOG_OFF
#define XB_LOG(severity) LOG_STREAM(severity)
#else
#define XB_LOG(severity) DUMMY_LOG
#endif

#define XB_LOGD XB_LOG(DEBUG)
#define XB_LOGI XB_LOG(INFO)
#define XB_LOGW XB_LOG(WARNING)
#define XB_LOGE XB_LOG(ERROR_)
#define XB_LOGFATAL XB_LOG(FATAL)