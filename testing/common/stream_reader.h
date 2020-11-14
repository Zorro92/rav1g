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
#include <memory>
#include <vector>
#include <list>
#include "frame_queue.h"
#include "common/ivfdec.h"
#include "common/webmdec.h"
#include "common/obudec.h"
#include "common/mp4parser.h"
#include "mkvparser/mkvparser.h"
#include "mkvparser/mkvreader.h"
#include "log.h"

class StreamReader;

class ReaderBuffer {
 public:
  ReaderBuffer(StreamReader* releaser) : buffer_(0), size_(0), releaser_(releaser) {}
  ~ReaderBuffer() {
    XB_LOGD << "~ReaderBuffer";
    delete[] buffer_;
  }
  int Put(const uint8_t* src, uint32_t size, int last = 0) {
    if (src && size) {
      if (size > capacity_) {
        XB_LOGD << "ReaderBuffer::Put " << size;
        delete[] buffer_;
        buffer_ = new (std::nothrow) uint8_t[size];
        if (!buffer_) return -1;
        capacity_ = size;
      }
      memcpy(buffer_, src, size);
      size_ = size;
    } else {
      delete[] buffer_;  // eof case
      buffer_ = 0;
      size_ = 0;
      capacity_ = 0;
    }
    isFinal_ = last;
    return 0;
  }
  uint8_t* Data() { return buffer_; }
  uint32_t Size() { return size_; }
  int IsFinal() const { return isFinal_; }

 private:
  uint8_t* buffer_ = 0;
  uint32_t size_ = 0;
  uint32_t capacity_ = 0;
  uint32_t isFinal_ = 0;
  StreamReader* releaser_;
  friend class ReadBufferWrapper;
};

typedef std::shared_ptr<ReaderBuffer> reader_buffer_ptr;
class ReadBufferWrapper {
 public:
  ReadBufferWrapper(reader_buffer_ptr buffer = reader_buffer_ptr(0)) : buffer_(buffer) {}
  ~ReadBufferWrapper();
  ReadBufferWrapper& operator=(reader_buffer_ptr other) {
    InternalRelease();
    buffer_ = other;
    return *this;
  }
  void Reset() {
    InternalRelease();
    buffer_ = 0;
  }
  ReaderBuffer* operator*() { return buffer_.get(); }
  operator bool() { return buffer_ != 0; }
  ReaderBuffer* operator->() { return buffer_.get(); }

 private:
  void InternalRelease();

 private:
  reader_buffer_ptr buffer_;
};

enum StreamReaderMode { srmodeAsync = 0, srmodeSync = 1 };

class StreamReader {
 public:
  StreamReader(uint32_t mode, uint32_t buffers_num = 20);
  ~StreamReader() {
    Stop();
    if (aom_input_ctx_.file) fclose(aom_input_ctx_.file);
    if (mp4_context_) destroy_mp4_ctx(mp4_context_);
    if (aom_input_ctx_.file_type == FILE_TYPE_IVF)
      ivf_free_buffer(&buf_);
    else if (aom_input_ctx_.file_type == FILE_TYPE_WEBM)
      webm_free(&webm_ctx_);
    else if (aom_input_ctx_.file_type == FILE_TYPE_OBU)
      obudec_free(&obu_ctx_);
  }
  int Start(std::string& source_path);

  void Stop() {
    if (!stop_) {
      stop_ = 1;
      if (mode_ == srmodeAsync) {
        reader_queue_.Push(0);
        reading_thread_.join();
      }
    }
  }
  reader_buffer_ptr GetData();
  void ReleaseData(reader_buffer_ptr data);
  size_t GetCount();
  double FrameRate() {
    if (aom_input_ctx_.framerate.denominator)
      return (double)aom_input_ctx_.framerate.numerator / (double)aom_input_ctx_.framerate.denominator;
    else
      return 30.0;
  }

 private:
  int readFrame(uint8_t** buf, size_t* bytes_in_buffer);
  static void ThreadFunc(StreamReader* ptr) { ptr->ReadLoop(); }
  int ReadLoop();

  std::string sourec_file_;
  AvxInputContext aom_input_ctx_ = {};
  WebmInputContext webm_ctx_ = {};
  ObuDecInputContext obu_ctx_ = {};
  mp4context* mp4_context_ = 0;
  volatile int stop_ = 1;
  volatile int started_ = 0;
  std::thread reading_thread_;
  std::mutex cs_;
  myqueue<reader_buffer_ptr> reader_queue_;
  myqueue<reader_buffer_ptr> ready_queue_;
  int eof_ = 0;
  size_t input_buffer_capacity_ = 0;
  uint8_t* buf_ = 0;
  uint32_t mode_ = 0;
  friend class ReaderBuffer;
};