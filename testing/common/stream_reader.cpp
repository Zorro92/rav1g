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

#include "stream_reader.h"
#include "log.h"

void ReadBufferWrapper::InternalRelease() {
  if (buffer_) buffer_->releaser_->ReleaseData(buffer_);
  buffer_.reset();
}
ReadBufferWrapper::~ReadBufferWrapper() { InternalRelease(); }

void StreamReader::ReleaseData(reader_buffer_ptr data) {
  if (data) reader_queue_.Push(data);
}

int StreamReader::readFrame(uint8_t **buf, size_t *bytes_in_buffer) {
  switch (aom_input_ctx_.file_type) {
    case FILE_TYPE_WEBM:
      return webm_read_frame(&webm_ctx_, buf, bytes_in_buffer, &input_buffer_capacity_);
    case FILE_TYPE_IVF:
      return ivf_read_frame(aom_input_ctx_.file, buf, bytes_in_buffer, &input_buffer_capacity_, NULL);
    case FILE_TYPE_OBU:
      return obudec_read_temporal_unit(&obu_ctx_, buf, bytes_in_buffer, &input_buffer_capacity_);
    case FILE_TYPE_MP4:
      return mp4_read_frame(mp4_context_, buf, bytes_in_buffer, &input_buffer_capacity_);
    default:
      return 1;
  }
}

StreamReader::StreamReader(uint32_t mode, uint32_t buffers_num)
    : reader_queue_(buffers_num), ready_queue_(buffers_num + 1), mode_(mode) {
  for (uint32_t i = 0; i < buffers_num; i++) {
    reader_queue_.Push(reader_buffer_ptr(new ReaderBuffer(this)));
  }
}

int StreamReader::Start(std::string &source_path) {
  sourec_file_ = source_path;
  aom_input_ctx_.filename = sourec_file_.c_str();
  aom_input_ctx_.file = fopen(aom_input_ctx_.filename, "rb");
  obu_ctx_.avx_ctx = &aom_input_ctx_;
  mp4_context_ = create_mp4_ctx();
  mp4_context_->file = aom_input_ctx_.file;
  if (!aom_input_ctx_.file) return -1;
  if (file_is_ivf(&aom_input_ctx_))
    aom_input_ctx_.file_type = FILE_TYPE_IVF;
  else if (file_is_webm(&webm_ctx_, &aom_input_ctx_)) {
    aom_input_ctx_.file_type = FILE_TYPE_WEBM;
    if (webm_guess_framerate(&webm_ctx_, &aom_input_ctx_)) {
      XB_LOGE << "Failed to guess framerate -- error parsing. webm file?";
      fclose(aom_input_ctx_.file);
      aom_input_ctx_.file = 0;
      return -1;
    }
  } else if (file_is_obu(&obu_ctx_))
    aom_input_ctx_.file_type = FILE_TYPE_OBU;
  else if (file_is_mp4(mp4_context_, &aom_input_ctx_))
    aom_input_ctx_.file_type = FILE_TYPE_MP4;
  else {
    XB_LOGE << "Unsupported input file type";
    fclose(aom_input_ctx_.file);
    aom_input_ctx_.file = 0;
    return -1;
  }
  if (mode_ == srmodeAsync) {
    stop_ = 0;
    eof_ = 0;
    reading_thread_ = std::thread(ThreadFunc, this);
  }
  return 0;
}

int StreamReader::ReadLoop() {
  started_ = 1;
  while (!stop_) {
    size_t buf_size = 0;
    reader_buffer_ptr buffer = reader_queue_.Pull();
    if (!buffer || readFrame(&buf_, &buf_size)) {
      break;
    } else {
      int ret = buffer->Put(buf_, (uint32_t)buf_size);
      if (ret) {
        XB_LOGE << "Out of memory while allocating " << buf_size << " bytes";
        ready_queue_.Push(0);
        return 1;
      }
      ready_queue_.Push(buffer);
    }
  }
  ready_queue_.Push(0);
  if (aom_input_ctx_.file) {
    fclose(aom_input_ctx_.file);
    aom_input_ctx_.file = 0;
  }
  return 0;
}

reader_buffer_ptr StreamReader::GetData() {
  if (mode_ == srmodeAsync) {
    reader_buffer_ptr buffer = ready_queue_.Pull();
    if (!buffer) {
      return 0;
    } else
      return buffer;
  } else {
    size_t buf_size = 0;
    reader_buffer_ptr buffer(reader_queue_.Pull());
    if (!buffer || readFrame(&buf_, &buf_size)) {
      if (aom_input_ctx_.file) {
        fclose(aom_input_ctx_.file);
        aom_input_ctx_.file = 0;
      }
      return 0;
    } else {
      int ret = buffer->Put(buf_, (uint32_t)buf_size);
      if (ret) {
        XB_LOGE << "Out of memory";
        return 0;
      }
    }
    return buffer;
  }
}

size_t StreamReader::GetCount() { return ready_queue_.Size(); }
