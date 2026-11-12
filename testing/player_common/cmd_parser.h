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
#ifndef _CMD_PARSER_H_
#define _CMD_PARSER_H_

#include <chrono>
#include <thread>
#include <string>
#include <vector>

#define PLAYER_QUEUE_SIZE 16
// -slots: the decoder's pipeline slots (aom_codec_dec_cfg_t::frame_slots).
#define PLAYER_SLOTS_DEFAULT 2
#define PLAYER_SLOTS_MIN 2
#define PLAYER_SLOTS_MAX 4
// -lowmemparts: the largest value, all three reductions (AV1_LOW_MEMORY_ALL of aom/aom_decoder.h), and what
// a task without -lowmem and -lowmemparts gets: part 1, the shared back buffer.
#define PLAYER_LOWMEM_ALL 7
#define PLAYER_LOWMEM_DEFAULT 1
// The size the decoder is created for with -nofitsize, and for a stream that is larger than this.
#define PLAYER_DECODER_WIDTH 4096
#define PLAYER_DECODER_HEIGHT 2176
#define PLAYER_DECODER_BITDEPTH 10

struct one_task : public std::vector<std::string> {
  std::string command_line_;
  one_task(std::string& command_line) : command_line_(command_line) {}
};

class CParameters {
 public:
  struct Parameters {
    std::string command_line_;
    std::string source_file_;
    std::string source_dir_;
    std::string bad_conform_dir_;
    std::string bad_conform_file_;
    std::string view_list_;
    std::string target_file_;
    std::string use_log_;
    std::string save_wrong_yuv_;
    std::string save_md5_;
    // -dumpstages: the frame list as given and as numbers; -dumpdir: where the dumps go (under LocalState).
    std::string dumpstages_;
    std::string dumpdir_;
    std::vector<int> dump_frames_;
    bool isDirectory_;
    bool isSubList_;
    bool showIt_;
    bool rawDecode_;
    int threads_;
    int loops_;
    int limit_;
    bool progress_;
    int start_frame_;
    int frate_;
    bool needReadBack_;
    bool md5_;
    bool md5_frame_check_;
    int scheme_;
    int player_queue_size_;
    int extra_fb_num_;
    int dec_task_queue_depth_;
    bool hdr10x3_;
    bool autoSyncAu_;
    bool nonwait_sync_;
    bool show_drops_;
    bool conformance_;
    bool noninterop_;
    bool recreate_;
    bool gputime_;
    bool nopresent_;
    // The effective choice after Parse(): true unless -cdefv1 was given.
    bool cdefv2_;
    bool cdefv1_;
    bool intrav2_;
    bool intraopt_;
    int slots_;
    bool asyncclear_;
    // The effective choice after Parse(): true unless -nofitsize was given.
    bool fitsize_;
    bool nofitsize_;
    bool lowmem_;
    // The parts in effect after Parse(): PLAYER_LOWMEM_DEFAULT, the value of -lowmemparts, or all with -lowmem.
    int lowmem_parts_;
    bool noguard_;
    // Experiments (docs/test-player.md): -lfhsplit 0|1|2, -intraedgefix.
    int lfhsplit_;
    bool intraedgefix_;
  };
  enum arg_type { atSingle, atInt, atString };
  enum arg_need { anOptional, anMandatory, anAtLeastOne };

  struct ParamStruct {
    const char* arg_name;
    arg_type type;
    arg_need need;
    const char* descr;
    const char* exclusive;
    void* pValue;
  };

  CParameters(const char* read_root = "", const char* write_root = "")
      : read_root_path_(read_root), write_root_path_(write_root) {
    Reset();
  }
  int Parse(one_task& args, std::string& Err);
  void Reset();
  void GetHelp(int pos, const char* arg, std::string& errMsg, const ParamStruct* arr, int size);
  const Parameters& GetParams() const { return params_; }
  Parameters* GetParamsPtr() { return &params_; }

 protected:
  Parameters params_;
  std::string read_root_path_;
  std::string write_root_path_;
};

#endif  //_CMD_PARSER_H_