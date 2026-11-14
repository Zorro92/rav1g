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

#include "cmd_parser.h"
#include "common/stage_dump.h"
#include <stdio.h>
#include <stdlib.h>
#include <map>

int CParameters::Parse(one_task& args, std::string& Err) {
  const ParamStruct avaiting[] = {
      {"i", atString, anAtLeastOne, "source file path", "", &params_.source_file_},
      {"o", atString, anOptional, "target YUV file path", "output", &params_.target_file_},
      {"dir", atString, anAtLeastOne, "source directory", "", &params_.source_dir_},
      {"t", atInt, anOptional,
       "threads that parse; 0 or no flag: the library's default for the frame size (1 up to 2560x1440, 2 above)", "",
       &params_.threads_},
      {"loop", atInt, anOptional, "loops", "", &params_.loops_},
      {"limit", atInt, anOptional, "decoding frame limit", "", &params_.limit_},
      {"progress", atSingle, anOptional, "show decoding time for each frame", "", &params_.progress_},
      {"save_md5", atString, anOptional, "Save md5 for each frame", "", &params_.save_md5_},
      {"md5_frame_check", atSingle, anOptional, "Calculate md5 for each frame & compare it with md5 file", "",
       &params_.md5_frame_check_},
      {"start", atInt, anOptional, "Start from frame No", "", &params_.start_frame_},
      {"save_wrong_yuv", atString, anOptional,
       "with -md5_frame_check: write each frame whose md5 differs to <dir>/<stream>_<frame>.yuv", "",
       &params_.save_wrong_yuv_},
      {"dumpstages", atString, anOptional,
       "frame list like 0,3,10-12 (decoding order, hidden frames count): write the buffers behind every GPU "
       "stage of these frames to <dumpdir>/<stream>_f<n>.stages",
       "", &params_.dumpstages_},
      {"dumpdir", atString, anOptional, "directory of -dumpstages under LocalState, default /dumps", "",
       &params_.dumpdir_},
      {"frate", atInt, anOptional,
       "Frame rate. 0 - webm header framerate(for webm only), -1 - maximum frate, default 0", "", &params_.frate_},
      {"dropinfo", atSingle, anOptional, "Show all drops", "", &params_.show_drops_},
      {"queue_size", atInt, anOptional, "Player frame queue size. Default 16", "", &params_.player_queue_size_},
      {"conformance", atSingle, anOptional, "Do conformance test for directory", "", &params_.conformance_},
      {"try10x3", atSingle, anOptional, "Use 10_10_10_2 display mode. Default false", "", &params_.hdr10x3_},
      {"r", atSingle, anOptional, "Recreate decoder for every file", "", &params_.recreate_},
      {"gputime", atSingle, anOptional, "per-stage GPU/CPU timing, summary in the log", "", &params_.gputime_},
      {"gputimedetail", atSingle, anOptional,
       "-gputime with the inter prediction breakdown: a timestamp behind every section, work counters", "",
       &params_.gputimedetail_},
      {"gputimeisolate", atSingle, anOptional,
       "-gputimedetail with a UAV barrier and a timestamp behind every inter dispatch (same output, slower)", "",
       &params_.gputimeisolate_},
      {"nopresent", atSingle, anOptional, "release decoded frames without presenting them", "",
       &params_.nopresent_},
      {"cdefv2", atSingle, anOptional, "CDEF through cdef_filter_v2.hlsl (the default; same output)", "cdef",
       &params_.cdefv2_},
      {"cdefv1", atSingle, anOptional, "CDEF through the original cdef_filter.hlsl (same output)", "cdef",
       &params_.cdefv1_},
      {"intrav2", atSingle, anOptional, "intra prediction through intra_v2.hlsl (same output)", "",
       &params_.intrav2_},
      {"intraopt", atSingle, anOptional, "original intra_main shader built with FXC optimisation (same source)", "",
       &params_.intraopt_},
      {"slots", atInt, anOptional,
       "decoder pipeline slots (frames in flight), 2..4. Default: what the library gives the frames in parse, one "
       "more than there are",
       "", &params_.slots_},
      {"parseframes", atInt, anOptional,
       "frames whose parse may run at the same time, 1..4 and at most the threads (-t); 0 or no flag: the library's "
       "default for the frame size, 1 up to 2560x1440 and 2 above (same output)",
       "", &params_.parse_frames_},
      {"gpuclear", atSingle, anOptional,
       "intra wavefront in the slot's grid, cleared by the GPU at the end of the frame's command list (same output)",
       "", &params_.gpuclear_},
      {"cpuclear", atSingle, anOptional,
       "intra wavefront in the slot's grid, cleared by the GPU completion thread (the default up to 0.2.144)", "",
       &params_.cpuclear_},
      {"asyncclear", atSingle, anOptional, "the older name of -cpuclear", "", &params_.asyncclear_flag_},
      {"syncclear", atSingle, anOptional,
       "intra wavefront in the slot's grid, cleared by the decode thread (the default up to 0.2.139)", "",
       &params_.syncclear_},
      {"fitsize", atSingle, anOptional,
       "create the decoder for the stream's size and bit depth (the default)", "fitsize", &params_.fitsize_},
      {"nofitsize", atSingle, anOptional, "create the decoder for 4096x2176 10-bit whatever the stream's size is",
       "fitsize", &params_.nofitsize_},
      {"lowmem", atSingle, anOptional,
       "smaller decoder: shared back buffer, smaller mode info and coefficient shares (same output)", "",
       &params_.lowmem_},
      {"lowmemparts", atInt, anOptional,
       "memory reductions in effect: 1 shared back buffer + 2 mode info shares + 4 coefficient shares. Default 1, "
       "0 for none",
       "", &params_.lowmem_parts_},
      {"lfhsplit", atInt, anOptional,
       "horizontal loop filter: 1 even rows of blocks, UAV barrier, odd rows; 2 every row of blocks on its "
       "own. Default 0, one dispatch per plane (same output)",
       "", &params_.lfhsplit_},
      {"intraedgefix", atSingle, anOptional,
       "8-bit intra: the above edge filter stops where the 10-bit one does (same output)", "",
       &params_.intraedgefix_},
      {"interfast", atSingle, anOptional,
       "inter prediction by the fraction of the motion vector: a copy, one filter pass, or both (same output)", "",
       &params_.interfast_},
      {"interfastlists", atSingle, anOptional,
       "the block lists of -interfast with the original inter shaders on them (same output)", "",
       &params_.interfastlists_},
      {"alllayers", atSingle, anOptional,
       "a stream with spatial layers: output the frame of every layer of a temporal unit (default: one frame "
       "per temporal unit, the highest layer it has)",
       "", &params_.all_layers_},
      {"oppoint", atInt, anOptional, "a stream with layers: decode operating point n of its sequence header. Default 0",
       "", &params_.oppoint_},
      {"noguard", atSingle, anOptional,
       "create the decoder without the memory budget guard (a creation that does not fit loses the D3D12 device)",
       "", &params_.noguard_},
      {"headroom", atInt, anOptional,
       "the MiB the decoder's budget guard should leave free behind the decoder (Rav1gSettings::memory_headroom); "
       "a decoder that would leave less is created with fewer slots and frames in parse. Default 0: the "
       "library's 128; 48 or less: only the margin of the guard, as up to package 0.2.156",
       "", &params_.headroom_mib_},
      {"budget", atInt, anOptional,
       "where the system gives the app no memory limit (rav1g-dec -playersmoke): model one, this many MiB above "
       "what the app uses without a decoder, for the decoder's budget guard. Ignored in the app",
       "", &params_.budget_mib_},
      {"retain", atInt, anOptional,
       "with -budget: a destroyed decoder leaves up to this many MiB charged to the app until the next decoder "
       "exists, as the console does",
       "", &params_.retain_mib_},
      {"parseopt", atString, anOptional,
       "the switches of rav1d's parse, separated by commas (same output): nolfmask, rav1d leaves out the masks "
       "of its own loop filter; tilethread, a tile stays on its thread between its superblock rows. Both are on "
       "by default, and a list that names one of them, none or default says which are on (none: neither; "
       "nolfmask or tilethread: only that one). time, with -gputime the calls of the sink are timed, and "
       "latelists, the block lists of a tile are counted at its end and not superblock by superblock, and "
       "cleargrid, the mode info grid is cleared for every frame, change nothing else",
       "", &params_.parseopt_},
      {"continue_on_error", atSingle, anOptional,
       "after a decode call that fails, go on with the next temporal unit in the same decoder instead of "
       "ending the task, as an application does with a damaged stream; the log counts the errors",
       "", &params_.continue_on_error_}};
  params_.command_line_ = args.command_line_;
  const int list_length = sizeof(avaiting) / sizeof(ParamStruct);
  bool has_value[list_length] = {false};
  Err = "";
  for (std::vector<std::string>::iterator iter = args.begin(); iter != args.end(); iter++) {
    const char* cur = iter->c_str();
    int par;
    for (par = 0; par < list_length; par++) {
      if (cur[0] != '-') {
        int i = (int)(iter - args.begin());
        GetHelp(i, iter->c_str(), Err, avaiting, list_length);
        return i;
      }
      if (strcmp(&cur[1], avaiting[par].arg_name) == 0) {
        switch (avaiting[par].type) {
          case atString:
            iter++;
            *static_cast<std::string*>(avaiting[par].pValue) += *iter;
            break;
          case atSingle:
            *static_cast<bool*>(avaiting[par].pValue) = true;
            break;
          case atInt:
            iter++;
            *static_cast<int*>(avaiting[par].pValue) = atoi(iter->c_str());
            break;
        }
        has_value[par] = true;
        break;
      }
    }
    if (par == list_length) {
      int i = (int)(iter - args.begin());
      GetHelp(i, iter->c_str(), Err, avaiting, list_length);
      return i;
    }
  }
  int oneof = 0;
  std::vector<int> idxs;
  std::map<std::string, int> exclusives;
  for (int i = 0; i < list_length; i++) {
    if (avaiting[i].need == anMandatory && !has_value[i]) {
      Err += "You MUST specify arg ";
      Err += avaiting[i].arg_name;
      Err += " value.\nUse \n";
      GetHelp(0, 0, Err, avaiting, list_length);
      return -1;
    } else if (avaiting[i].need == anAtLeastOne) {
      idxs.push_back(i);
      if (has_value[i]) oneof++;
    }
    if (strlen(avaiting[i].exclusive) && has_value[i]) {
      std::map<std::string, int>::iterator iter = exclusives.find(avaiting[i].exclusive);
      if (iter != exclusives.end()) {
        Err += "You can't use following args together: ";
        Err += "\"";
        Err += avaiting[iter->second].arg_name;
        Err += "\" ";
        Err += "\"";
        Err += avaiting[i].arg_name;
        Err += "\"";
        return -1;
      }
      exclusives.insert(std::pair<std::string, int>(avaiting[i].exclusive, i));
    }
  }
  if (!idxs.empty() && !oneof) {
    Err += "You MUST specify at least one of the following args: ";
    for (std::vector<int>::iterator iter = idxs.begin(); iter < idxs.end(); iter++) {
      Err += avaiting[*iter].arg_name;
      Err += ", ";
    }
    Err += "\nUse \n";
    GetHelp(0, 0, Err, avaiting, list_length);
    return -1;
  }

  // -gpuclear, -cpuclear (-asyncclear), -syncclear: who clears the grids of a slot, with the wavefront in
  // its grid. Without one of them nothing has to be cleared. Two of them on one line is an error of the
  // task.
  const bool cpuclear = params_.cpuclear_ || params_.asyncclear_flag_;
  if ((params_.gpuclear_ ? 1 : 0) + (cpuclear ? 1 : 0) + (params_.syncclear_ ? 1 : 0) > 1) {
    Err += "-gpuclear, -cpuclear (-asyncclear) and -syncclear are the choices of one switch";
    return -1;
  }
  params_.clears_ = params_.gpuclear_ ? PLAYER_CLEARS_GPU
                                      : cpuclear ? PLAYER_CLEARS_COMPLETION_THREAD
                                                 : params_.syncclear_ ? PLAYER_CLEARS_DECODE_THREAD
                                                                      : PLAYER_CLEARS_NOT_NEEDED;

  // -parseopt: words separated by commas; a word that is none of them is an error of the task. A word
  // that names the two default switches (nolfmask, tilethread, none, default) replaces the default of the
  // two; time, latelists and cleargrid add to whatever they are (rav1g::ParserOptions::from_words reads the list the
  // same way).
  params_.parser_options_ = 0;
  bool ways_named = false;
  for (size_t at = 0; at <= params_.parseopt_.size() && !params_.parseopt_.empty();) {
    size_t end = params_.parseopt_.find(',', at);
    if (end == std::string::npos) end = params_.parseopt_.size();
    const std::string word = params_.parseopt_.substr(at, end - at);
    if (word == "time") {
      params_.parser_options_ |= PLAYER_PARSEOPT_TIME_CALLS;
    } else if (word == "latelists") {
      params_.parser_options_ |= PLAYER_PARSEOPT_LATE_LISTS;
    } else if (word == "cleargrid") {
      params_.parser_options_ |= PLAYER_PARSEOPT_CLEAR_GRID;
    } else if (word == "nolfmask") {
      params_.parser_options_ |= PLAYER_PARSEOPT_NO_LF_MASKS;
      ways_named = true;
    } else if (word == "tilethread") {
      params_.parser_options_ |= PLAYER_PARSEOPT_TILE_KEEPS_THREAD;
      ways_named = true;
    } else if (word == "default") {
      params_.parser_options_ |= PLAYER_PARSEOPT_DEFAULT;
      ways_named = true;
    } else if (word == "none") {
      ways_named = true;
    } else {
      Err += "-parseopt takes nolfmask, tilethread, time, latelists, cleargrid, default or none, separated by commas, not \"";
      Err += word;
      Err += "\"";
      return -1;
    }
    at = end + 1;
  }
  if (!ways_named) params_.parser_options_ |= PLAYER_PARSEOPT_DEFAULT;

  // A displayed frame stays held until its replacement arrives. Headless output needs only one.
  const int min_queue = params_.nopresent_ ? 1 : 2;
  if (params_.player_queue_size_ < min_queue) {
    Err += "-queue_size must be at least " + std::to_string(min_queue) +
           (params_.nopresent_ ? " with -nopresent." : " when presenting frames.");
    return -1;
  }
  // -slots: 0 is what Reset() leaves, and means that the task did not say: the library then gives the
  // decoder the slots of its frames in parse (one more than there are, or what its budget guard leaves).
  params_.slots_given_ = params_.slots_ != 0;
  if (params_.slots_given_ && (params_.slots_ < PLAYER_SLOTS_MIN || params_.slots_ > PLAYER_SLOTS_MAX)) {
    // Refused, not clamped: a timing run must not report numbers for a slot count nobody asked for.
    char ss[96];
    sprintf(ss, "-slots %d is not allowed: the decoder takes %d to %d slots.", params_.slots_, PLAYER_SLOTS_MIN,
            PLAYER_SLOTS_MAX);
    Err += ss;
    return -1;
  }
  if (params_.parse_frames_ < 0 || params_.parse_frames_ > PLAYER_PARSE_FRAMES_MAX) {
    char ss[128];
    sprintf(ss, "-parseframes %d is not allowed: 1 to %d frames in parse, or 0 for the library's default.",
            params_.parse_frames_, PLAYER_PARSE_FRAMES_MAX);
    Err += ss;
    return -1;
  }
  // -t: 0 is what Reset() leaves: the library's default for the frame size.
  if (params_.threads_ < 0) params_.threads_ = 0;

  if (params_.lowmem_parts_ < 0 || params_.lowmem_parts_ > PLAYER_LOWMEM_ALL) {
    char ss[96];
    sprintf(ss, "-lowmemparts %d is not allowed: the sum of 1, 2 and 4, at most %d.", params_.lowmem_parts_,
            PLAYER_LOWMEM_ALL);
    Err += ss;
    return -1;
  }
  if (params_.lfhsplit_ < 0 || params_.lfhsplit_ > 2) {
    char ss[96];
    sprintf(ss, "-lfhsplit %d is not allowed: 0, 1 or 2.", params_.lfhsplit_);
    Err += ss;
    return -1;
  }
  if (!params_.dumpstages_.empty() && !ParseStageDumpFrames(params_.dumpstages_, &params_.dump_frames_)) {
    Err += "-dumpstages " + params_.dumpstages_ + " is not a frame list like 0,3,10-12 of at most 256 frames.";
    return -1;
  }
  // The inter breakdown is part of the timing run.
  params_.gputime_detail_ = params_.gputimeisolate_ ? 2 : params_.gputimedetail_ ? 1 : 0;
  if (params_.gputime_detail_) params_.gputime_ = true;
  if (params_.dumpdir_.empty()) params_.dumpdir_ = "/dumps";
  params_.dumpdir_ = write_root_path_ + params_.dumpdir_;
  // -lowmem is all parts; -lowmemparts alone says exactly which ones (0: none); neither: the default, part 1.
  if (params_.lowmem_) params_.lowmem_parts_ = PLAYER_LOWMEM_ALL;
  // The defaults that have an opt-out flag. -cdefv2 and -fitsize are still accepted and change nothing;
  // giving a flag together with its opposite was refused above.
  params_.cdefv2_ = !params_.cdefv1_;
  params_.fitsize_ = !params_.nofitsize_;

  params_.isDirectory_ = (params_.source_file_.empty() && !params_.source_dir_.empty());
  params_.isSubList_ = (params_.source_file_.empty() && params_.source_dir_.empty() && !params_.view_list_.empty());
  params_.needReadBack_ = !params_.target_file_.empty() || params_.md5_ || params_.md5_frame_check_ ||
                          !params_.save_md5_.empty() || params_.conformance_ || params_.noninterop_;
  if (!params_.source_file_.empty()) params_.source_file_ = read_root_path_ + params_.source_file_;
  if (!params_.target_file_.empty()) params_.target_file_ = write_root_path_ + params_.target_file_;
  if (!params_.source_dir_.empty()) params_.source_dir_ = read_root_path_ + params_.source_dir_;
  if (!params_.save_md5_.empty()) params_.save_md5_ = write_root_path_ + params_.save_md5_;
  if (!params_.save_wrong_yuv_.empty()) params_.save_wrong_yuv_ = write_root_path_ + params_.save_wrong_yuv_;
  if (params_.conformance_) {
    params_.frate_ = -1;
    params_.md5_ = true;
  }
  return 0;
}

void CParameters::Reset() {
  params_.source_file_.clear();
  params_.source_dir_.clear();
  params_.bad_conform_dir_.clear();
  params_.view_list_.clear();
  params_.use_log_.clear();
  params_.save_wrong_yuv_.clear();
  params_.save_md5_.clear();
  params_.dumpstages_.clear();
  params_.dumpdir_.clear();
  params_.dump_frames_.clear();
  params_.isDirectory_ = false;
  params_.isSubList_ = false;
  params_.showIt_ = false;
  // 0: the task has no -t; the library's default for the frame size.
  params_.threads_ = 0;
  params_.loops_ = 1;
  params_.limit_ = -1;
  params_.progress_ = false;
  params_.start_frame_ = 0;
  params_.frate_ = 0;
  params_.needReadBack_ = false;
  params_.md5_ = false;
  params_.md5_frame_check_ = false;
  params_.scheme_ = 0;
  params_.player_queue_size_ = PLAYER_QUEUE_SIZE;
  params_.extra_fb_num_ = 0;
  params_.dec_task_queue_depth_ = 6;
  params_.hdr10x3_ = 0;
  params_.autoSyncAu_ = false;
  params_.nonwait_sync_ = false;
  params_.rawDecode_ = false;
  params_.show_drops_ = false;
  params_.conformance_ = false;
  params_.noninterop_ = false;
  params_.recreate_ = false;
  params_.gputime_ = false;
  params_.gputimedetail_ = false;
  params_.gputimeisolate_ = false;
  params_.gputime_detail_ = 0;
  params_.nopresent_ = false;
  params_.cdefv2_ = true;
  params_.cdefv1_ = false;
  params_.intrav2_ = false;
  params_.intraopt_ = false;
  // 0: the task has no -slots (Parse() makes the default of it).
  params_.slots_ = 0;
  params_.slots_given_ = false;
  params_.parse_frames_ = PLAYER_PARSE_FRAMES_DEFAULT;
  params_.clears_ = PLAYER_CLEARS_NOT_NEEDED;
  params_.gpuclear_ = false;
  params_.cpuclear_ = false;
  params_.asyncclear_flag_ = false;
  params_.syncclear_ = false;
  params_.fitsize_ = true;
  params_.nofitsize_ = false;
  params_.lowmem_ = false;
  params_.lowmem_parts_ = PLAYER_LOWMEM_DEFAULT;
  params_.noguard_ = false;
  params_.headroom_mib_ = 0;
  params_.budget_mib_ = -1;
  params_.retain_mib_ = 0;
  params_.continue_on_error_ = false;
  params_.parseopt_.clear();
  params_.parser_options_ = PLAYER_PARSEOPT_DEFAULT;
  params_.lfhsplit_ = 0;
  params_.intraedgefix_ = false;
  params_.interfast_ = false;
  params_.interfastlists_ = false;
  params_.all_layers_ = false;
  params_.oppoint_ = 0;
}

void CParameters::GetHelp(int pos, const char* arg, std::string& errMsg, const ParamStruct* arr, int size) {
  if (arg) {
    char ss[128];
    sprintf(ss, "Invalid command string.\nError in arg %d (%s).\nUse \n", pos, arg);
    errMsg += ss;
  }
  errMsg += "{-i <source file path> | -dir <source directory>}\n";
  for (int i = 2; i < size; i++) {
    const ParamStruct& item = arr[i];
    errMsg += " [-";
    errMsg += item.arg_name;
    if (item.type != atSingle) {
      errMsg += " <";
      errMsg += item.descr;
      errMsg += ">";
    }
    errMsg += "]";
    if (item.type == atSingle) {
      errMsg += "\t";
      errMsg += item.descr;
    }
    errMsg += "\n";
  }
  // Handled before the task lines are parsed (decoder_creator.cpp), so it is not in the list above.
  errMsg += " [-sm6test]\tShader Model 6 test before the tasks; may be the only thing on its line\n";
  errMsg += "\n";
}
