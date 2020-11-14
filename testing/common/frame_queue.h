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
#include <thread>
#include <mutex>
#include <condition_variable>
#include <queue>

template <class T>
class myqueue {
 public:
  myqueue(unsigned int cap = 5) : cap_(cap) {}
  void Push(T value) {
    std::unique_lock<std::mutex> lock(cs_);
    cv_.wait(lock, [this] { return this->queue_.size() < this->cap_; });
    queue_.push(value);
    cv_.notify_one();
  }
  void PushWait(T value) {
    Push(value);
    std::unique_lock<std::mutex> lock(cs_get_);
    wait_ = true;
    cv_get_.wait(lock, [this] { return !this->wait_; });
  }
  T Pull() {
    std::unique_lock<std::mutex> lock(cs_);
    cv_.wait(lock, [this] { return !this->queue_.empty(); });
    T r = queue_.front();
    queue_.pop();
    cv_.notify_one();
    return r;
  }
  T PullLimited(uint32_t max_size) {
    std::unique_lock<std::mutex> lock(cs_);
    cv_.wait(lock, [this] { return this->cap_ - this->queue_.size() >= max_size; });
    T r = queue_.front();
    queue_.pop();
    cv_.notify_one();
    return r;
  }
  T& FrontUnsafe() {
    std::unique_lock<std::mutex> lock(cs_);
    cv_.wait(lock, [this] { return !this->queue_.empty(); });
    T& r = queue_.front();
    return r;
  }
  void EndWait() {
    std::unique_lock<std::mutex> lock(cs_get_);
    wait_ = false;
    cv_get_.notify_one();
  }
  bool IsEmpty() {
    std::lock_guard<std::mutex> lock(cs_);
    return queue_.empty();
  }
  bool notFull() {
    std::lock_guard<std::mutex> lock(cs_);
    return queue_.size() < cap_;
  }
  size_t Size() {
    std::lock_guard<std::mutex> lock(cs_);
    return queue_.size();
  }
  template <class Predicate>
  void DoForAll(int waitNotEmpty, Predicate func) {
    std::unique_lock<std::mutex> lock(cs_);
    if (waitNotEmpty) cv_.wait(lock, [this] { return !this->queue_.empty(); });
    for (auto iter = queue_._Get_container().begin(); iter != queue_._Get_container().end(); iter++) {
      T t = *iter;
      if (!func(t)) break;
    }
  }

 private:
  std::queue<T> queue_;
  std::mutex cs_;
  std::condition_variable cv_;
  std::mutex cs_get_;
  std::condition_variable cv_get_;
  unsigned int cap_;
  bool wait_ = false;
};
