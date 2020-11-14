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
#include <stdint.h>
#include <assert.h>

#define timers_number 10
class StreamTimer {
 public:
  StreamTimer() : frame_no_(0) { QueryPerformanceFrequency(&frequency_); }

  uint64_t GetCurrent(int idx, uint64_t* duration) {
    assert(idx >= 0 && idx < timers_number);
    if (st2_[idx].QuadPart == 0) {
      QueryPerformanceCounter(&st2_[idx]);
      last2_[idx] = st2_[idx];
      *duration = 0;
      return 0;
    }
    LARGE_INTEGER cur;
    QueryPerformanceCounter(&cur);
    *duration = (cur.QuadPart - last2_[idx].QuadPart) * 1000000 / frequency_.QuadPart;
    uint64_t ct = (cur.QuadPart - st2_[idx].QuadPart) * 1000000 / frequency_.QuadPart;
    last2_[idx] = cur;
    return ct;
  }

 private:
  uint32_t frame_no_ = 0;

  LARGE_INTEGER st2_[timers_number] = {};
  LARGE_INTEGER last2_[timers_number] = {};
  LARGE_INTEGER frequency_;
};