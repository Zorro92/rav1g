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

#include <wrl.h>
#include "log.h"

const char* log_priority_names[] = {
    "DEBUG", "INFO", "WARNING", "ERROR", "FATAL",
};
logging::LogPriority g_priority = logging::LOG_ERROR;
std::string g_log_file;

namespace logging {
void set_log_priority(logging::LogPriority priority) { g_priority = priority; }
void set_log_file(const char* file_path) { g_log_file = file_path; }

LogMessage::LogMessage(const char* file, int line, LogPriority priority)
    : priority_(priority), file_(file), line_(line) {
  Init(file_, line_);
}
LogMessage::~LogMessage() {
  if (priority_ >= g_priority) {
    stream_ << std::endl;
    std::string str_newline(stream_.str());
    std::unique_lock<std::mutex> lock(cs_);
    RawLog(str_newline);
  }
}
void LogMessage::Init(const char* file, int line) {
  std::string filename(file);
  size_t last_slash_pos = filename.find_last_of("\\/");
  if (last_slash_pos != std::string::npos) {
    filename.erase(0, last_slash_pos + 1);
  }
  stream_ << '[';
  stream_ << std::this_thread::get_id() << ':';
  stream_ << ((double)clock() / (double)CLOCKS_PER_SEC) << ':';
  stream_ << log_priority_names[priority_];
  stream_ << ":" << filename << "(" << line << ")] ";
  message_start_ = stream_.tellp();
}
void LogMessage::RawLog(std::string& message) {
#if (WINAPI_FAMILY == WINAPI_FAMILY_APP)
  OutputDebugStringA((message + "\n").c_str());
#else
  printf("%s\n", message.c_str());
#endif
  if (!g_log_file.empty()) {
    FILE* f = fopen(g_log_file.c_str(), "a+");
    if (f) {
      fprintf(f, "%s", message.c_str());
      fclose(f);
    }
  }
}

}  // namespace logging