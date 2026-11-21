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
// (A task without -slots uses the library default.)
// The clears of a pipeline slot (-gpuclear, -cpuclear, -syncclear): the values of RAV1G_CLEARS_* of
// rav1g.h (decoder.cpp holds them against it).
#define PLAYER_CLEARS_NOT_NEEDED 0
#define PLAYER_CLEARS_GPU 1
#define PLAYER_CLEARS_COMPLETION_THREAD 2
#define PLAYER_CLEARS_DECODE_THREAD 3
#define PLAYER_SLOTS_MIN 2
#define PLAYER_SLOTS_MAX 4
// -parseframes: the frames whose parse may run at the same time (Rav1gSettings::frames_in_parse), 1 to
// RAV1G_FRAMES_IN_PARSE_MAX; 0 is the library's default for the frame size (1 up to 2560x1440, 2 above),
// which a task without the flag gets.
#define PLAYER_PARSE_FRAMES_DEFAULT 0
#define PLAYER_PARSE_FRAMES_MAX 4
// -lowmemparts: the largest value, all three reductions (AV1_LOW_MEMORY_ALL of aom/aom_decoder.h), and what
// a task without -lowmem and -lowmemparts gets: the library's default for the decoder's size
// (RAV1G_LOW_MEMORY_DEFAULT of rav1g.h, which decoder.cpp holds it against: part 1, the shared back buffer,
// and above 1920x1088 every part: 2, the mode info pool at half, 4, the coefficient pool at four fifths,
// and 32, the frame buffer bound at the recording).
#define PLAYER_LOWMEM_ALL 7
// Every part -lowmemparts takes: those of -lowmem, and 32, the frame buffer bound at the recording
// (RAV1G_LOW_MEMORY_LATE_FRAME_BUFFER), which -lowmem does not have.
#define PLAYER_LOWMEM_PARTS 39
#define PLAYER_LOWMEM_DEFAULT 16
// -parseopt: the words as bits (RAV1G_PARSER_OPTION_* of rav1g.h, which decoder.cpp holds them against),
// and what a task without -parseopt gets: nolfmask, tilethread, gpucdef and gpugrid (RAV1G_PARSER_OPTIONS_DEFAULT).
#define PLAYER_PARSEOPT_NO_LF_MASKS 2
#define PLAYER_PARSEOPT_TILE_KEEPS_THREAD 4
#define PLAYER_PARSEOPT_TIME_CALLS 8
#define PLAYER_PARSEOPT_LATE_LISTS 16
#define PLAYER_PARSEOPT_CLEAR_GRID 32
#define PLAYER_PARSEOPT_GPU_CDEF_LISTS 512
// gpugrid: the mode info grid of a frame is built by a pass of the GPU from the frame's records and not by the
// parse threads (off by default; such a decoder has gpucdef on whatever the list says).
#define PLAYER_PARSEOPT_GPU_GRID 2048
#define PLAYER_PARSEOPT_GPU_PRED_LISTS 4096
#define PLAYER_PARSEOPT_DEFAULT \
  (PLAYER_PARSEOPT_NO_LF_MASKS | PLAYER_PARSEOPT_TILE_KEEPS_THREAD | PLAYER_PARSEOPT_GPU_CDEF_LISTS | PLAYER_PARSEOPT_GPU_GRID)
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
    // -memstats: the decoder's memory statistics (rav1g_set_memory_stats; docs/memory.md).
    bool memstats_;
    int start_frame_;
    int frate_;
    bool needReadBack_;
    bool md5_;
    bool md5_frame_check_;
    bool annexb_;
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
    // -gputimedetail, -gputimeisolate: the inter prediction breakdown of the timing run. After Parse():
    // gputime_detail_ is 0 (none), 1 (section timestamps and work counters) or 2 (also a UAV barrier and a
    // timestamp behind every dispatch), and either flag switches gputime_ on.
    bool gputimedetail_;
    bool gputimeisolate_;
    int gputime_detail_;
    bool nopresent_;
    // -output and -interop. After Parse(): semi_planar_ is 0 (the planes), 1 (one NV12 or P010 texture an
    // image, which shaders may write) or 2 (one that they may not); interop_ makes it at least 1.
    std::string output_;
    bool interop_;
    int semi_planar_;
    // The effective choice after Parse(): true unless -cdefv1 was given.
    bool cdefv2_;
    bool cdefv1_;
    bool cdeflegacy_;
    bool cdeflistslegacy_;
    bool intrav2_;
    bool intraopt_;
    bool intraod_;
    bool intrafilterlegacy_;
    bool intraschedulelegacy_;
    bool intrabanklegacy_;
    bool intrasteplegacy_;
    bool lrlegacy_;
    bool fglegacy_;
    bool copylegacy_;
    bool idctlegacy_;
    bool clearlegacy_;
    bool interlegacy_;
    // -slots: the pipeline slots the task asks for. 0 when the task has no -slots: the library then gives
    // the decoder the slots of its frames in parse.
    int slots_;
    // The task has -slots (set by Parse()).
    bool slots_given_;
    // -parseframes: the frames in parse the task asks for; 0 is the library's default.
    int parse_frames_;
    // The effective choice after Parse(): what a frame does about the clears of its slot's grids, one of
    // PLAYER_CLEARS_*. Without a flag nothing has to be cleared; -gpuclear, -cpuclear (-asyncclear is its
    // older name) and -syncclear keep the wavefront in the slot's grid and say who clears it.
    int clears_;
    bool gpuclear_;
    bool cpuclear_;
    bool asyncclear_flag_;
    bool syncclear_;
    // The effective choice after Parse(): true unless -nofitsize was given.
    bool fitsize_;
    bool nofitsize_;
    bool lowmem_;
    // The parts in effect after Parse(): PLAYER_LOWMEM_DEFAULT, the value of -lowmemparts, or all with -lowmem.
    int lowmem_parts_;
    bool noguard_;
    // -headroom: what the budget guard should leave free behind the decoder, MiB; 0: the library's default.
    int headroom_mib_;
    // -budget, -retain: the model of the app's memory where the system gives the app no limit
    // (decoder.cpp, "The app's memory where the system has no limit for it"). -1: no model.
    int budget_mib_;
    int retain_mib_;
    // Names of the parser switches. Naming a default switch, none or default selects the defaults
    // explicitly. The other switches, including gpublists, add to that selection.
    std::string parseopt_;
    unsigned parser_options_;
    // -continue_on_error: a decode call that fails does not end the task; the next temporal units go to
    // the same decoder.
    bool continue_on_error_;
    // Experiments (docs/test-player.md): -lfhsplit 0|1|2, -intraedgefix.
    bool lflegacy_;
    int lfhsplit_;
    bool intraedgefix_;
    // -interfast: inter blocks by the fraction of their motion vectors (docs/test-player.md);
    // -interfastlists: the same lists with the original shaders on them.
    bool interfast_;
    bool interfastlists_;
    // Streams with layers: -alllayers (AV1D_SET_OUTPUT_ALL_LAYERS), -oppoint <n> (AV1D_SET_OPERATING_POINT).
    bool all_layers_;
    int oppoint_;
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
