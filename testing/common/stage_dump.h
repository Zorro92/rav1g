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
// frame later. When the stream starts a sequence that takes a new decoder, the frame numbers start again;
// the files of the frames behind the n-th such start are <stream>.seq<n>_f<frame number>.stages.
//
// A dump is a header of 384 bytes and its data (little endian). Format 2 ("RAV1STG2"); format 1 ("RAV1STG1")
// is the same without layout_size and without dumps of kind 2:
//
//   char     magic[8]         "RAV1STG2"
//   uint32_t header_size      384
//   int32_t  frame_number     decoder frame counter (frames in decoding order, hidden frames included)
//   int32_t  index            position of the dump in the command list
//   int32_t  kind             0 samples, 1 residuals (signed 16 bit), 2 data (a list or a table)
//   int32_t  bytes_per_sample 0 for kind 2
//   int32_t  bit_depth
//   int32_t  is_sef           the dump comes from the command list of a show_existing_frame
//   uint32_t layout_size      kind 2: the data starts with this many bytes of text that describe the rest
//   uint32_t planes[3][4]     offset of the first visible sample in the data, stride in bytes, visible width,
//                             visible height (0: no such plane; kind 2 has none)
//   uint64_t data_size        layout_size included
//   char     stage[32]        zero padded
//   char     info[264]        zero padded: one line about the frame
//   uint8_t  data[data_size]  kind 2: the layout text, then the buffer; else the buffer as it was stored
//
// The layout text of a dump of kind 2 is lines of space separated key=value fields. The first line starts with
// buffer=<name> and has the fields of the whole buffer; the others start with a word (list, run, plane) and
// describe a part. All numbers are decimal. The buffers (the stage name is the buffer name):
//
//   mi_grid    entry=8 mi_stride mi_cols mi_rows pool_base record=224
//              One entry of 8 bytes per 4x4 unit, rows of mi_stride entries: the address of the unit's mode
//              info record, 0 for none. Its low 32 bits minus pool_base, divided by record, is the index of
//              the record in the mode info pool. (The addresses differ from run to run; the records do not.)
//   mi_pool    record=224, and one line "run first=<pool index> count=<records>" per tile
//              The records of the runs, one run behind the other. A record is MB_MODE_INFO of
//              libav1/dx/shaders/mode_info.h.
//   pred_blocks       record=16 count     the prediction block records of gen_pred_blocks.hlsl
//   pred_blocks_warp  record=48 count     its warped block records
//   lf_params  gen_size lf_size bit_depth
//              GenLfData (libav1/dx/loopfilters.cpp; the constants of gen_lf_blocks_*.hlsl), gen_size bytes,
//              then LoopfilterData (the constants of loopfilter_*.hlsl), lf_size bytes; 32-bit words.
//   lf_blocks  record=32 lists=6, and six lines
//              "list dir=<vert|horz> plane=<0..2> first cols count mi_cols mi_rows on"
//              The edge lists of the loop filter. A list is count records from record first; on=0: the plane
//              is not filtered and the list was not written for this frame. A record is a block: sixteen
//              edges of 16 bits, level in bits 0-5 (0: not filtered), filter in bits 6-7 (0: 4 taps, 1: 6,
//              2: 8, 3: 14), step to the next edge of the block in bits 8-15 (in 4-sample units; 0 ends the
//              block). Block b of a vert list is the 4 rows from row 4 * (b / cols) and the 64 columns from
//              column 64 * (b % cols) of the plane, and its edge e is the vertical edge at column
//              64 * (b % cols) + 4 * e. Block b of a horz list is the 4 columns from column 4 * (b % cols)
//              and the 64 rows from row 64 * (b / cols), and its edge e the horizontal edge at row
//              64 * (b / cols) + 4 * e.
//   cdef_info  index_size index_stride index_rows skips_stride skip_cols skip_rows
//              index_size bytes: one word per 64x64 block, rows of index_stride (the block's strength
//              index); then one word per 8x8 block, rows of skips_stride (1: all of its 4x4 units are
//              skipped), of which skip_cols by skip_rows are written for the frame.
//   lr_info    units type_size=16 wiener_size=64, and per restored plane "units plane first cols rows unit_size"
//              units records of four words (type 0 none, 1 Wiener, 2 self guided; the two self guided
//              weights; the self guided set), then units records of sixteen words (horizontal and vertical
//              Wiener filter, written for a Wiener unit only).

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
    sequence_ = 0;
    dumps_ = 0;
    bytes_ = 0;
    error_.clear();
    lines_.clear();
  }

  // The decoder was replaced for a new sequence of the stream (after the drain of the one before): the frame
  // numbers start again, and so do the file names.
  void NextSequence() {
    std::lock_guard<std::mutex> lock(cs_);
    ++sequence_;
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
    char number[64];
    if (sequence_)
      snprintf(number, sizeof(number), ".seq%d_f%d.stages", sequence_, dump->frame_number);
    else
      snprintf(number, sizeof(number), "_f%d.stages", dump->frame_number);
    const std::string path = dir_ + "/" + name_ + number;
    const size_t layout_size = (dump->kind == 2 && dump->layout) ? strlen(dump->layout) : 0;
    const uint64_t data_size = (uint64_t)layout_size + (uint64_t)dump->size;
    // The first dump of the frame's own command list starts the file; everything else goes behind it.
    FILE* file = fopen(path.c_str(), (dump->index == 0 && !dump->is_sef) ? "wb" : "ab");
    unsigned char header[384];
    memset(header, 0, sizeof(header));
    memcpy(header, "RAV1STG2", 8);
    Put32(header + 8, (uint32_t)sizeof(header));
    Put32(header + 12, (uint32_t)dump->frame_number);
    Put32(header + 16, (uint32_t)dump->index);
    Put32(header + 20, (uint32_t)dump->kind);
    Put32(header + 24, (uint32_t)dump->bytes_per_sample);
    Put32(header + 28, (uint32_t)dump->bit_depth);
    Put32(header + 32, (uint32_t)dump->is_sef);
    Put32(header + 36, (uint32_t)layout_size);
    for (int p = 0; p < 3; ++p) {
      Put32(header + 40 + 16 * p + 0, dump->planes[p].offset);
      Put32(header + 40 + 16 * p + 4, dump->planes[p].stride);
      Put32(header + 40 + 16 * p + 8, dump->planes[p].width);
      Put32(header + 40 + 16 * p + 12, dump->planes[p].height);
    }
    Put32(header + 88, (uint32_t)(data_size & 0xffffffffu));
    Put32(header + 92, (uint32_t)(data_size >> 32));
    strncpy((char*)header + 96, dump->stage ? dump->stage : "", 31);
    strncpy((char*)header + 128, dump->info ? dump->info : "", 255);
    bool ok = file != NULL;
    if (ok) ok = fwrite(header, 1, sizeof(header), file) == sizeof(header);
    if (ok && layout_size) ok = fwrite(dump->layout, 1, layout_size, file) == layout_size;
    if (ok) ok = fwrite(dump->data, 1, dump->size, file) == dump->size;
    if (file && fclose(file)) ok = false;
    char line[512];
    if (dump->kind == 2)
      snprintf(line, sizeof(line), "DUMPSTAGES frame %d%s stage %d %s: %llu bytes, data -> %s%s", dump->frame_number,
               dump->is_sef ? " (show_existing_frame)" : "", dump->index, dump->stage ? dump->stage : "?",
               (unsigned long long)dump->size, path.c_str(), ok ? "" : " FAILED (cannot write)");
    else
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
  int sequence_ = 0;
  int dumps_ = 0;
  uint64_t bytes_ = 0;
  std::string error_;
  std::vector<std::string> lines_;
};
