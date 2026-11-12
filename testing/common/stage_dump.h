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

// -dumpstages of the test player and of warp_decoder: the frame list of the flag, and the files the dumps
// of the decoder (av1_set_stage_dump in dx/av1_perf.h) are written to. Both programs write the same files, so
// that testing/stage_dump/stage_dump.py can compare a run of one with a run of the other.
//
// One file per frame, <dir>/<stream>_f<frame number>.stages: the dumps of the frame one behind the other, in
// the order of its command list, and behind them the output dump of a show_existing_frame that shows the
// frame later. A dump is a header of 384 bytes and the buffer as it was stored (little endian):
//
//   char     magic[8]         "RAV1STG1"
//   uint32_t header_size      384
//   int32_t  frame_number     decoder frame counter (frames in decoding order, hidden frames included)
//   int32_t  index            position of the dump in the command list
//   int32_t  kind             0 samples, 1 residuals (signed 16 bit)
//   int32_t  bytes_per_sample
//   int32_t  bit_depth
//   int32_t  is_sef           the dump comes from the command list of a show_existing_frame
//   int32_t  reserved
//   uint32_t planes[3][4]     offset of the first visible sample in the data, stride in bytes, visible width,
//                             visible height (0: no such plane)
//   uint64_t data_size
//   char     stage[32]        zero padded
//   char     info[264]        zero padded: one line about the frame
//   uint8_t  data[data_size]

#pragma once

#include <stdint.h>
#include <stdio.h>
#include <string.h>

#include <mutex>
#include <string>
#include <vector>

#include "dx/av1_perf.h"

// "0,3,10-12" -> 0 3 10 11 12. False when the text is not such a list, or has more than 256 frames.
inline bool ParseStageDumpFrames(const std::string& text, std::vector<int>* frames) {
  frames->clear();
  size_t pos = 0;
  while (pos < text.size()) {
    size_t end = text.find(',', pos);
    if (end == std::string::npos) end = text.size();
    const std::string item = text.substr(pos, end - pos);
    pos = end + 1;
    int first = 0, last = 0;
    char tail = 0;
    if (sscanf(item.c_str(), "%d-%d%c", &first, &last, &tail) == 2) {
    } else if (sscanf(item.c_str(), "%d%c", &first, &tail) == 1) {
      last = first;
    } else {
      return false;
    }
    if (first < 0 || last < first) return false;
    for (int f = first; f <= last; ++f) {
      if (frames->size() >= 256) return false;
      frames->push_back(f);
    }
  }
  return !frames->empty();
}

class StageDumpWriter {
 public:
  // |dir| must exist. |name| is what the files of the stream are called.
  void Start(const std::string& dir, const std::string& name) {
    std::lock_guard<std::mutex> lock(cs_);
    dir_ = dir;
    name_ = name;
    dumps_ = 0;
    bytes_ = 0;
    error_.clear();
    lines_.clear();
  }

  // The decoder's callback (GPU completion thread); |priv| is the writer.
  static void Callback(void* priv, const av1_stage_dump* dump) { static_cast<StageDumpWriter*>(priv)->Write(dump); }

  // After the decoder was drained: what was written. One line per dump for the log, and the first error.
  std::vector<std::string> TakeLines() {
    std::lock_guard<std::mutex> lock(cs_);
    std::vector<std::string> lines;
    lines.swap(lines_);
    return lines;
  }
  std::string Error() {
    std::lock_guard<std::mutex> lock(cs_);
    return error_;
  }
  int Dumps() {
    std::lock_guard<std::mutex> lock(cs_);
    return dumps_;
  }
  uint64_t Bytes() {
    std::lock_guard<std::mutex> lock(cs_);
    return bytes_;
  }

 private:
  void Write(const av1_stage_dump* dump) {
    std::lock_guard<std::mutex> lock(cs_);
    char number[32];
    snprintf(number, sizeof(number), "_f%d.stages", dump->frame_number);
    const std::string path = dir_ + "/" + name_ + number;
    // The first dump of the frame's own command list starts the file; everything else goes behind it.
    FILE* file = fopen(path.c_str(), (dump->index == 0 && !dump->is_sef) ? "wb" : "ab");
    unsigned char header[384];
    memset(header, 0, sizeof(header));
    memcpy(header, "RAV1STG1", 8);
    Put32(header + 8, (uint32_t)sizeof(header));
    Put32(header + 12, (uint32_t)dump->frame_number);
    Put32(header + 16, (uint32_t)dump->index);
    Put32(header + 20, (uint32_t)dump->kind);
    Put32(header + 24, (uint32_t)dump->bytes_per_sample);
    Put32(header + 28, (uint32_t)dump->bit_depth);
    Put32(header + 32, (uint32_t)dump->is_sef);
    for (int p = 0; p < 3; ++p) {
      Put32(header + 40 + 16 * p + 0, dump->planes[p].offset);
      Put32(header + 40 + 16 * p + 4, dump->planes[p].stride);
      Put32(header + 40 + 16 * p + 8, dump->planes[p].width);
      Put32(header + 40 + 16 * p + 12, dump->planes[p].height);
    }
    Put32(header + 88, (uint32_t)((uint64_t)dump->size & 0xffffffffu));
    Put32(header + 92, (uint32_t)((uint64_t)dump->size >> 32));
    strncpy((char*)header + 96, dump->stage ? dump->stage : "", 31);
    strncpy((char*)header + 128, dump->info ? dump->info : "", 255);
    bool ok = file != NULL;
    if (ok) ok = fwrite(header, 1, sizeof(header), file) == sizeof(header);
    if (ok) ok = fwrite(dump->data, 1, dump->size, file) == dump->size;
    if (file && fclose(file)) ok = false;
    char line[512];
    snprintf(line, sizeof(line), "DUMPSTAGES frame %d%s stage %d %s: %llu bytes, %ux%u %d-bit -> %s%s",
             dump->frame_number, dump->is_sef ? " (show_existing_frame)" : "", dump->index,
             dump->stage ? dump->stage : "?", (unsigned long long)dump->size, dump->planes[0].width,
             dump->planes[0].height, dump->bit_depth, path.c_str(), ok ? "" : " FAILED (cannot write)");
    lines_.push_back(line);
    if (ok) {
      ++dumps_;
      bytes_ += dump->size;
    } else if (error_.empty()) {
      error_ = "cannot write " + path;
    }
  }

  static void Put32(unsigned char* dst, uint32_t v) {
    dst[0] = (unsigned char)(v & 0xff);
    dst[1] = (unsigned char)((v >> 8) & 0xff);
    dst[2] = (unsigned char)((v >> 16) & 0xff);
    dst[3] = (unsigned char)((v >> 24) & 0xff);
  }

  std::mutex cs_;
  std::string dir_;
  std::string name_;
  int dumps_ = 0;
  uint64_t bytes_ = 0;
  std::string error_;
  std::vector<std::string> lines_;
};
