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

#include <stdio.h>
#include <stdint.h>
#include <string.h>

#include "common/tools_common.h"

typedef struct mp4context {
  void* parser;
  FILE* file;
  uint8_t* buffer;
  size_t buffer_capacity;
  size_t bytes_buffered;
  int frate_denominator;
  int frate_numerator;
} mp4context;

#ifdef __cplusplus

extern "C" {
mp4context* create_mp4_ctx();
int file_is_mp4(mp4context* ctx, AvxInputContext* input_ctx);
int mp4_read_frame(mp4context* ctx, uint8_t** buf, size_t* bytes_in_buf, size_t* buffer_size);
int destroy_mp4_ctx(mp4context* ctx);
}

#include <vector>
#include <list>

class Mp4Parser {
 public:
  ~Mp4Parser() {
    if (need_fclose_) CloseFile();
  }
  void CloseFile() {
    if (file_) fclose(file_);
    file_ = 0;
  }
  int OpenFile(const char* path);
  int OpenFile(FILE* file);
  uint32_t GetSize();
  int Read(uint8_t* buffer);
  AvxRational FrameRate();
  uint32_t Width() { return width_; }
  uint32_t Height() { return height_; }

 protected:
  struct mp4atom {
    uint32_t offset;
    uint32_t size;
    uint64_t ext_size;
    uint8_t name[5];
    mp4atom() { memset(this, 0, sizeof(mp4atom)); }
  };

  struct mvhdAtom {
    mp4atom atom;
    uint32_t version;
    uint32_t reserved;
    uint32_t creation_time;
    uint32_t modification_time;
    uint32_t timescale;
  };

  struct mdhdAtom {
    mp4atom atom;
    uint32_t version;
    uint32_t reserved;
    uint32_t creation_time;
    uint32_t modification_time;
    uint32_t timescale;
  };

  struct tfhdAtom {
    mp4atom atom;
    uint32_t version;
    uint32_t flags;
    uint32_t track_id;
    // All of the following are optional fields
    uint64_t base_data_offset;
    uint32_t sample_desc_index;
    uint32_t defaultSampleDuration;
    uint32_t defaultSampleSize;
    uint32_t defaultSampleFlags;
  };

  struct trunEntry {
    uint32_t duration;
    uint32_t size;
    uint32_t flags;
    int64_t CompositionTimeOffset;
    uint64_t file_offset;
    uint64_t time;
  };

  struct trunAtom {
    mp4atom atom;
    uint32_t version;
    uint32_t flags;
    uint32_t count;
    uint32_t data_offset;
    uint32_t first_sample_flags;
    std::vector<trunEntry> table;
    uint32_t table_pointer = 0;
    uint32_t valid = 0;
  };
  struct sidxEntry {
    uint32_t reference_type;
    uint32_t referenced_size;
    uint32_t subsegment_duration;
    uint32_t starts_with_SAP;
    uint32_t SAP_type;
    uint32_t SAP_delta_time;
  };
  struct sidxAtom {
    mp4atom atom;
    uint32_t version;
    uint32_t flags;
    uint32_t track_id;
    uint32_t timescale;
    uint64_t first_pts;
    uint64_t first_offset;
    uint32_t item_count;
    std::vector<sidxEntry> table;
  };
  struct sttsEntry {
    uint32_t sample_count;
    uint32_t sample_delta;
  };
  struct sttsAtom {
    mp4atom atom;
    uint32_t version;
    uint32_t flags;
    uint32_t entry_count;
    uint32_t total_samples;
    std::vector<sttsEntry> table;
  };
  struct stscEntry {
    uint32_t first_chunk;
    uint32_t samples_per_chunk;
    uint32_t sample_description_index;
  };
  struct stscAtom {
    mp4atom atom;
    uint32_t version;
    uint32_t flags;
    uint32_t entry_count;
    std::vector<stscEntry> table;
  };
  struct stszAtom {
    mp4atom atom;
    uint32_t version;
    uint32_t flags;
    uint32_t size;
    uint32_t entry_count;
    std::vector<uint32_t> table;
  };
  struct stcoAtom {
    mp4atom atom;
    uint32_t version;
    uint32_t flags;
    uint32_t size;
    uint32_t entry_count;
    std::vector<uint32_t> table;
  };

 protected:
  int readAtom(mp4atom* atom);
  int readUINT8(uint32_t* val);
  int readUINT16(uint16_t* val);
  int readUINT16(uint32_t* val);
  int readUINT24(uint32_t* val);
  int readUINT32(uint32_t* val);
  int readUINT32(uint64_t* val);
  int readUINT64(uint64_t* val);
  int readName(uint8_t* buf);
  int findAtom(const char* name, mp4atom* atom);
  int readAtom(const char* name, mp4atom* atom);
  int parse_stsd();
  int parse_trun();
  int parse_tfhd();
  int parse_mvhd();
  int parse_mdhd();
  int parse_sidx();
  int parse_stts();
  int parse_stsc();
  int parse_stsz();
  int parse_stco();
  int calcChunk();

 private:
  FILE* file_ = 0;
  int need_fclose_ = 0;
  uint16_t width_ = 0;
  uint16_t height_ = 0;
  int default_base_is_moof_ = 0;
  int is_fragmented_ = 0;
  int is_qt_format_ = 0;

  uint32_t current_chunk_ = 0;
  uint32_t current_chunk_id_ = 0;
  uint32_t first_in_current_chunk_ = 0;
  uint32_t current_sample_ = 0;

  mp4atom ftyp_;
  mp4atom moov_atom_;
  mvhdAtom mvhd_atom_;
  mp4atom any_atom_1_;
  mp4atom any_atom_2_;
  mp4atom any_atom_3_;
  mp4atom any_atom_4_;
  mp4atom any_atom_5_;
  mp4atom any_atom_6_;
  mp4atom stbl_atom_;
  mp4atom stsd_atom_;
  mp4atom av01_atom_;
  mp4atom current_moof_;
  mp4atom moof_atom_;
  trunAtom trun_atom_;
  tfhdAtom tfhd_atom_;
  sidxAtom sidx_atom_;
  sttsAtom stts_atom_;
  stscAtom stsc_atom_;
  stszAtom stsz_atom_;
  stcoAtom stco_atom_;
  mdhdAtom mdhd_atom_;
};
#endif
