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
#include <stdio.h>
#include <stdlib.h>
#include <map>

int CParameters::Parse(one_task& args, std::string& Err) {
  const ParamStruct avaiting[] = {
      {"i", atString, anAtLeastOne, "source file path", "", &params_.source_file_},
      {"o", atString, anOptional, "target YUV file path", "output", &params_.target_file_},
      {"dir", atString, anAtLeastOne, "source directory", "", &params_.source_dir_},
      {"t", atInt, anOptional, "threads", "", &params_.threads_},
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
      {"frate", atInt, anOptional,
       "Frame rate. 0 - webm header framerate(for webm only), -1 - maximum frate, default 0", "", &params_.frate_},
      {"dropinfo", atSingle, anOptional, "Show all drops", "", &params_.show_drops_},
      {"queue_size", atInt, anOptional, "Player frame queue size. Default 16", "", &params_.player_queue_size_},
      {"conformance", atSingle, anOptional, "Do conformance test for directory", "", &params_.conformance_},
      {"try10x3", atSingle, anOptional, "Use 10_10_10_2 display mode. Default false", "", &params_.hdr10x3_},
      {"r", atSingle, anOptional, "Recreate decoder for every file", "", &params_.recreate_},
      {"gputime", atSingle, anOptional, "per-stage GPU/CPU timing, summary in the log", "", &params_.gputime_},
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
      {"slots", atInt, anOptional, "decoder pipeline slots (frames in flight), 2..4. Default 2", "",
       &params_.slots_},
      {"asyncclear", atSingle, anOptional, "per-frame clears off the decode thread (same output)", "",
       &params_.asyncclear_},
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
      {"noguard", atSingle, anOptional,
       "create the decoder without the memory budget guard (a creation that does not fit loses the D3D12 device)",
       "", &params_.noguard_}};
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

  if (params_.slots_ < PLAYER_SLOTS_MIN || params_.slots_ > PLAYER_SLOTS_MAX) {
    // Refused, not clamped: a timing run must not report numbers for a slot count nobody asked for.
    char ss[96];
    sprintf(ss, "-slots %d is not allowed: the decoder takes %d to %d slots.", params_.slots_, PLAYER_SLOTS_MIN,
            PLAYER_SLOTS_MAX);
    Err += ss;
    return -1;
  }

  if (params_.lowmem_parts_ < 0 || params_.lowmem_parts_ > PLAYER_LOWMEM_ALL) {
    char ss[96];
    sprintf(ss, "-lowmemparts %d is not allowed: the sum of 1, 2 and 4, at most %d.", params_.lowmem_parts_,
            PLAYER_LOWMEM_ALL);
    Err += ss;
    return -1;
  }
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
  params_.isDirectory_ = false;
  params_.isSubList_ = false;
  params_.showIt_ = false;
  params_.threads_ = 1;
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
  params_.nopresent_ = false;
  params_.cdefv2_ = true;
  params_.cdefv1_ = false;
  params_.intrav2_ = false;
  params_.intraopt_ = false;
  params_.slots_ = PLAYER_SLOTS_DEFAULT;
  params_.asyncclear_ = false;
  params_.fitsize_ = true;
  params_.nofitsize_ = false;
  params_.lowmem_ = false;
  params_.lowmem_parts_ = PLAYER_LOWMEM_DEFAULT;
  params_.noguard_ = false;
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
  errMsg += "\n";
}
