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

#include "mp4parser.h"
#include <assert.h>

#define MOV_TRUN_DATA_OFFSET 0x01
#define MOV_TRUN_FIRST_SAMPLE_FLAGS 0x04
#define MOV_TRUN_SAMPLE_DURATION 0x100
#define MOV_TRUN_SAMPLE_SIZE 0x200
#define MOV_TRUN_SAMPLE_FLAGS 0x400
#define MOV_TRUN_SAMPLE_CTS 0x800

#define MOV_FRAG_SAMPLE_FLAG_DEGRADATION_PRIORITY_MASK 0x0000ffff
#define MOV_FRAG_SAMPLE_FLAG_IS_NON_SYNC 0x00010000
#define MOV_FRAG_SAMPLE_FLAG_PADDING_MASK 0x000e0000
#define MOV_FRAG_SAMPLE_FLAG_REDUNDANCY_MASK 0x00300000
#define MOV_FRAG_SAMPLE_FLAG_DEPENDED_MASK 0x00c00000
#define MOV_FRAG_SAMPLE_FLAG_DEPENDS_MASK 0x03000000

#define MOV_TFHD_BASE_DATA_OFFSET 0x01
#define MOV_TFHD_STSD_ID 0x02
#define MOV_TFHD_DEFAULT_DURATION 0x08
#define MOV_TFHD_DEFAULT_SIZE 0x10
#define MOV_TFHD_DEFAULT_FLAGS 0x20
#define MOV_TFHD_DURATION_IS_EMPTY 0x010000
#define MOV_TFHD_DEFAULT_BASE_IS_MOOF 0x020000

int Mp4Parser::findAtom(const char* name, mp4atom* atom) {
  int ret;
  while (1) {
    ret = readAtom(atom);
    if (ret) return ret;
    if (!strcmp((const char*)atom->name, name)) return 0;
    ret = fseek(file_, atom->offset + atom->size, SEEK_SET);
    if (ret) return ret;
  }
}

int Mp4Parser::readAtom(const char* name, mp4atom* atom) {
  int ret;
  ret = readAtom(atom);
  if (ret) return ret;
  return strcmp((const char*)atom->name, name);
}

int Mp4Parser::parse_stsd() {
  uint32_t val;
  int ret = readUINT32(&val);
  if (ret) return ret;
  ret = readUINT32(&val);
  if (ret || val != 1) return -1;
  mp4atom atom;
  ret = readAtom(&atom);
  if (ret) return ret;
  if (strcmp((const char*)atom.name, "av01")) return -1;
  fseek(file_, 24, SEEK_CUR);
  ret = readUINT16(&width_);
  if (ret) return ret;
  ret = readUINT16(&height_);
  if (ret) return ret;
  return 0;
}

int Mp4Parser::parse_stsc() {
  int ret = findAtom("stsc", &stsc_atom_.atom);
  if (ret) return ret;
  ret = readUINT8(&stsc_atom_.version);
  if (ret) return ret;
  ret = readUINT24(&stsc_atom_.flags);
  if (ret) return ret;
  ret = readUINT32(&stsc_atom_.entry_count);
  if (ret) return ret;
  for (uint32_t i = 0; i < stsc_atom_.entry_count; i++) {
    stscEntry stc;
    ret = readUINT32(&stc.first_chunk);
    if (ret) return ret;
    ret = readUINT32(&stc.samples_per_chunk);
    if (ret) return ret;
    ret = readUINT32(&stc.sample_description_index);
    if (ret) return ret;
    stsc_atom_.table.push_back(stc);
  }
  return 0;
}

int Mp4Parser::parse_stts() {
  int ret = findAtom("stts", &stts_atom_.atom);
  if (ret) return ret;
  ret = readUINT8(&stts_atom_.version);
  if (ret) return ret;
  ret = readUINT24(&stts_atom_.flags);
  if (ret) return ret;
  ret = readUINT32(&stts_atom_.entry_count);
  if (ret) return ret;
  stts_atom_.total_samples = 0;
  for (uint32_t i = 0; i < stts_atom_.entry_count; i++) {
    sttsEntry stts_entry;
    ret = readUINT32(&stts_entry.sample_count);
    if (ret) return ret;
    ret = readUINT32(&stts_entry.sample_delta);
    stts_atom_.table.push_back(stts_entry);
    stts_atom_.total_samples += stts_entry.sample_count;
  }
  return 0;
}

int Mp4Parser::parse_stsz() {
  int ret = findAtom("stsz", &stsz_atom_.atom);
  if (ret) return ret;
  ret = readUINT8(&stsz_atom_.version);
  if (ret) return ret;
  ret = readUINT24(&stsz_atom_.flags);
  if (ret) return ret;
  ret = readUINT32(&stsz_atom_.size);
  if (ret) return ret;
  ret = readUINT32(&stsz_atom_.entry_count);
  if (ret) return ret;
  for (uint32_t i = 0; i < stsz_atom_.entry_count; i++) {
    uint32_t stsz_entry;
    ret = readUINT32(&stsz_entry);
    stsz_atom_.table.push_back(stsz_entry);
  }
  return 0;
}

int Mp4Parser::parse_stco() {
  int ret = findAtom("stco", &stco_atom_.atom);
  if (ret) return ret;
  ret = readUINT8(&stco_atom_.version);
  if (ret) return ret;
  ret = readUINT24(&stco_atom_.flags);
  if (ret) return ret;
  ret = readUINT32(&stco_atom_.entry_count);
  if (ret) return ret;
  for (uint32_t i = 0; i < stsz_atom_.entry_count; i++) {
    uint32_t stco_entry;
    ret = readUINT32(&stco_entry);
    stco_atom_.table.push_back(stco_entry);
  }
  return 0;
}

int Mp4Parser::parse_sidx() {
  int ret = fseek(file_, moov_atom_.offset + moov_atom_.size, SEEK_SET);
  if (ret) return ret;
  ret = readAtom("sidx", &sidx_atom_.atom);
  if (ret) return 1;
  ret = readUINT8(&sidx_atom_.version);
  if (ret) return ret;
  ret = readUINT24(&sidx_atom_.flags);
  if (ret) return ret;
  ret = readUINT32(&sidx_atom_.track_id);
  if (ret) return ret;
  ret = readUINT32(&sidx_atom_.timescale);
  if (ret) return ret;
  if (sidx_atom_.version == 0) {
    ret = readUINT32(&sidx_atom_.first_pts);
    if (ret) return ret;
    ret = readUINT32(&sidx_atom_.first_offset);
    if (ret) return ret;
  } else {
    ret = readUINT64(&sidx_atom_.first_pts);
    if (ret) return ret;
    ret = readUINT64(&sidx_atom_.first_offset);
    if (ret) return ret;
  }
  uint16_t val;
  ret = readUINT16(&val);
  if (ret) return ret;
  ret = readUINT16(&sidx_atom_.item_count);
  if (ret) return ret;

  sidx_atom_.table.reserve(sidx_atom_.item_count);
  for (uint32_t i = 0; i < sidx_atom_.item_count; i++) {
    sidxEntry entry;
    uint32_t tmp;
    ret = readUINT32(&tmp);
    if (ret) return ret;
    entry.reference_type = (tmp & 0x80000000) >> 24;
    entry.referenced_size = tmp & 0x7fffffff;
    ret = readUINT32(&entry.subsegment_duration);
    if (ret) return ret;
    ret = readUINT32(&tmp);
    if (ret) return ret;
    entry.starts_with_SAP = (tmp & 0x80000000) >> 24;
    entry.SAP_type = (tmp & 0x70000000) >> 24;
    entry.SAP_delta_time = tmp & 0xfffffff;
    sidx_atom_.table.push_back(entry);
  }

  return 0;
}

int Mp4Parser::parse_trun() {
  mp4atom atom;
  int ret;
  ret = findAtom("moof", &current_moof_);
  if (ret) return ret;
  ret = findAtom("traf", &atom);
  if (ret) return ret;
  ret = findAtom("trun", &trun_atom_.atom);
  if (ret) return ret;
  ret = readUINT8(&trun_atom_.version);
  if (ret) return ret;
  ret = readUINT24(&trun_atom_.flags);
  if (ret) return ret;
  ret = readUINT32(&trun_atom_.count);
  if (ret) return ret;
  trun_atom_.data_offset = 0;
  if (trun_atom_.flags & MOV_TRUN_DATA_OFFSET) {
    ret = readUINT32(&trun_atom_.data_offset);
    if (ret) return ret;
  }
  uint64_t data_offset =
      trun_atom_.data_offset + (default_base_is_moof_ ? current_moof_.offset : tfhd_atom_.base_data_offset);
  trun_atom_.first_sample_flags = 0;
  if (trun_atom_.flags & MOV_TRUN_FIRST_SAMPLE_FLAGS) {
    ret = readUINT32(&trun_atom_.first_sample_flags);
    if (ret) return ret;
  }
  uint32_t prev_size = 0;
  uint64_t frame_time = 0;
  trun_atom_.table.clear();
  trun_atom_.table.reserve(trun_atom_.count);
  trun_atom_.table_pointer = 0;
  for (uint32_t i = 0; i < trun_atom_.count; i++) {
    trunEntry entry;
    data_offset += prev_size;
    if (trun_atom_.flags & MOV_TRUN_SAMPLE_DURATION) readUINT32(&entry.duration);
    if (trun_atom_.flags & MOV_TRUN_SAMPLE_SIZE) readUINT32(&entry.size);
    if (trun_atom_.flags & MOV_TRUN_SAMPLE_FLAGS) readUINT32(&entry.flags);
    if (trun_atom_.flags & MOV_TRUN_SAMPLE_CTS) {
      assert(0);
    }
    entry.file_offset = data_offset;
    entry.time = frame_time;
    frame_time += (trun_atom_.flags & MOV_TRUN_SAMPLE_DURATION) ? entry.duration : tfhd_atom_.defaultSampleDuration;
    prev_size = entry.size;
    trun_atom_.table.push_back(entry);
  }
  trun_atom_.valid = 1;
  return 0;
}

int Mp4Parser::parse_mvhd() {
  int ret = findAtom("mvhd", &mvhd_atom_.atom);
  if (ret) return ret;
  ret = readUINT8(&mvhd_atom_.version);
  if (ret) return ret;
  ret = readUINT24(&mvhd_atom_.reserved);
  if (ret) return ret;
  ret = readUINT32(&mvhd_atom_.creation_time);
  if (ret) return ret;
  ret = readUINT32(&mvhd_atom_.modification_time);
  if (ret) return ret;
  ret = readUINT32(&mvhd_atom_.timescale);
  if (ret) return ret;

  ret = fseek(file_, mvhd_atom_.atom.offset + mvhd_atom_.atom.size, SEEK_SET);
  if (ret) return ret;
  return 0;
}

int Mp4Parser::parse_mdhd() {
  int ret = findAtom("mdhd", &mdhd_atom_.atom);
  if (ret) return ret;
  ret = readUINT8(&mdhd_atom_.version);
  if (ret) return ret;
  ret = readUINT24(&mdhd_atom_.reserved);
  if (ret) return ret;
  ret = readUINT32(&mdhd_atom_.creation_time);
  if (ret) return ret;
  ret = readUINT32(&mdhd_atom_.modification_time);
  if (ret) return ret;
  ret = readUINT32(&mdhd_atom_.timescale);
  if (ret) return ret;

  ret = fseek(file_, mdhd_atom_.atom.offset + mdhd_atom_.atom.size, SEEK_SET);
  if (ret) return ret;
  return 0;
}

int Mp4Parser::parse_tfhd() {
  mp4atom atom;
  int ret = fseek(file_, moov_atom_.offset + moov_atom_.size, SEEK_SET);
  if (ret) return ret;
  ret = findAtom("moof", &moof_atom_);
  if (ret) return ret;
  ret = findAtom("traf", &atom);
  if (ret) return ret;
  ret = findAtom("tfhd", &tfhd_atom_.atom);
  if (ret) return ret;
  ret = readUINT8(&tfhd_atom_.version);
  if (ret) return ret;
  ret = readUINT24(&tfhd_atom_.flags);
  if (ret) return ret;
  ret = readUINT32(&tfhd_atom_.track_id);
  if (ret) return ret;
  if (MOV_TFHD_BASE_DATA_OFFSET & tfhd_atom_.flags) {
    ret = readUINT64(&tfhd_atom_.base_data_offset);
    if (ret) return ret;
  }
  if (MOV_TFHD_STSD_ID & tfhd_atom_.flags) {
    ret = readUINT32(&tfhd_atom_.sample_desc_index);
    if (ret) return ret;
  }
  if (MOV_TFHD_DEFAULT_DURATION & tfhd_atom_.flags) {
    ret = readUINT32(&tfhd_atom_.defaultSampleDuration);
    if (ret) return ret;
  }

  default_base_is_moof_ = MOV_TFHD_DEFAULT_BASE_IS_MOOF & tfhd_atom_.flags;

  return 0;
}

int Mp4Parser::OpenFile(FILE* file) {
  file_ = file;
  need_fclose_ = 0;
  int ret = readAtom(&ftyp_);
  if (ret) return ret;
  if (strcmp((const char*)ftyp_.name, "ftyp")) return -1;
  ret = fseek(file_, ftyp_.offset + ftyp_.size, SEEK_SET);
  if (ret) return ret;
  ret = findAtom("moov", &moov_atom_);
  if (ret) return ret;
  ret = parse_mvhd();
  if (ret) return ret;
  ret = findAtom("trak", &any_atom_1_);
  if (ret) return ret;
  ret = findAtom("mdia", &any_atom_2_);
  if (ret) return ret;
  ret = parse_mdhd();
  if (ret) return ret;
  ret = findAtom("minf", &any_atom_3_);
  if (ret) return ret;
  ret = findAtom("stbl", &stbl_atom_);
  if (ret) return ret;
  // next level
  ret = findAtom("stsd", &stsd_atom_);
  if (ret) return ret;
  ret = parse_stsd();
  if (ret) return ret;

  ret = fseek(file_, stsd_atom_.offset + stsd_atom_.size, SEEK_SET);
  ret = parse_stts();
  if (ret) return ret;
  if (stts_atom_.entry_count) {
    // quicktime format
    is_qt_format_ = 1;
    ret = fseek(file_, stbl_atom_.offset + 8, SEEK_SET);
    if (ret) return ret;

    ret = parse_stsc();
    if (ret) return ret;
    ret = parse_stsz();
    if (ret) return ret;
    ret = parse_stco();
    if (ret) return ret;
    return 0;
  } else {
    ret = fseek(file_, moov_atom_.offset + moov_atom_.size, SEEK_SET);
    ret = parse_tfhd();
    if (ret) return ret;

    ret = parse_sidx();
    if (ret != 1 && ret != 0) return ret;
    is_fragmented_ = ret == 0;
    ret = fseek(file_, moov_atom_.offset + moov_atom_.size, SEEK_SET);
    if (ret) return ret;
    ret = parse_trun();
    if (ret) return ret;
    return 0;
  }
}

int Mp4Parser::OpenFile(const char* path) {
  FILE* file = fopen(path, "rb");
  if (!file) {
    printf("Failed to open file %s\n", path);
    return -1;
  }
  need_fclose_ = 1;
  return OpenFile(file);
}

int Mp4Parser::readName(uint8_t* buf) {
  size_t read = fread(buf, 1, 4, file_);
  if (read != 4) return -1;
  buf[4] = 0;
  return 0;
}

int Mp4Parser::readUINT8(uint32_t* val) {
  uint8_t buf;
  size_t read = fread(&buf, 1, 1, file_);
  if (read != 1) return -1;
  *val = buf;
  return 0;
}

int Mp4Parser::readUINT24(uint32_t* val) {
  uint8_t buf[3];
  size_t read = fread(buf, 1, 3, file_);
  if (read != 3) return -1;
  *val = (buf[0] << 16) | (buf[1] << 8) | (buf[2]);
  return 0;
}

int Mp4Parser::readUINT16(uint16_t* val) {
  uint8_t buf[2];
  size_t read = fread(buf, 1, 2, file_);
  if (read != 2) return -1;
  *val = (buf[0] << 8) | (buf[1]);
  return 0;
}

int Mp4Parser::readUINT16(uint32_t* val) {
  uint8_t buf[2];
  size_t read = fread(buf, 1, 2, file_);
  if (read != 2) return -1;
  *val = (buf[0] << 8) | (buf[1]);
  return 0;
}

int Mp4Parser::readUINT32(uint32_t* val) {
  uint8_t buf[4];
  size_t read = fread(buf, 1, 4, file_);
  if (read != 4) return -1;
  *val = (buf[0] << 24) | (buf[1] << 16) | (buf[2] << 8) | (buf[3]);
  return 0;
}

int Mp4Parser::readUINT32(uint64_t* val) {
  uint8_t buf[4];
  size_t read = fread(buf, 1, 4, file_);
  if (read != 4) return -1;
  *val = (buf[0] << 24) | (buf[1] << 16) | (buf[2] << 8) | (buf[3]);
  return 0;
}

int Mp4Parser::readUINT64(uint64_t* val) {
  uint8_t buf[8];
  size_t read = fread(buf, 1, 8, file_);
  if (read != 8) return -1;
  *val = ((uint64_t)((buf[0] << 24) | (buf[1] << 16) | (buf[2] << 8) | (buf[3] << 0)) << 32) | (buf[4] << 24) |
         (buf[5] << 16) | (buf[6] << 8) | (buf[7]);
  return 0;
}

int Mp4Parser::readAtom(mp4atom* atom) {
  atom->offset = ftell(file_);
  int ret = readUINT32(&atom->size);
  if (ret) return ret;
  ret = readName(atom->name);
  if (ret) return ret;
  if (atom->size == 1) {
    ret = readUINT64(&atom->ext_size);
    if (ret) return ret;
  }
  return 0;
}

int Mp4Parser::Read(uint8_t* buffer) {
  if (is_qt_format_) {
    int ret = calcChunk();
    if (ret) return -1;
    uint32_t chunk_offset = stco_atom_.table[current_chunk_];
    uint32_t inchunk_offset = 0;
    for (uint32_t sample = first_in_current_chunk_; sample < current_sample_; sample++)
      inchunk_offset += stsz_atom_.table[sample];
    uint32_t file_offset = chunk_offset + inchunk_offset;
    fseek(file_, file_offset, SEEK_SET);
    size_t _read = fread(buffer, 1, stsz_atom_.table[current_sample_], file_);
    if (_read != stsz_atom_.table[current_sample_]) return -1;
    current_sample_++;
    return 0;
  } else {
    trunEntry& entry = trun_atom_.table[trun_atom_.table_pointer];
    fseek(file_, (long)entry.file_offset, SEEK_SET);
    size_t _read = fread(buffer, 1, entry.size, file_);
    if (_read != entry.size) return -1;
    trun_atom_.table_pointer++;
    return 0;
  }
}

int Mp4Parser::calcChunk() {
  while (1) {
    if (current_sample_ >= first_in_current_chunk_ &&
        current_sample_ < (first_in_current_chunk_ + stsc_atom_.table[current_chunk_id_].samples_per_chunk)) {
      return 0;
    }
    current_chunk_++;
    first_in_current_chunk_ += stsc_atom_.table[current_chunk_id_].samples_per_chunk;
    for (uint32_t ch_id = 0; ch_id < stsc_atom_.entry_count; ch_id++) {
      if (current_chunk_ >= (stsc_atom_.table[ch_id].first_chunk - 1))
        current_chunk_id_ = ch_id;
      else
        break;
    }
  }
  // never happen
  return -1;
}

uint32_t Mp4Parser::GetSize() {
  int ret;
  if (is_qt_format_) {
    if (current_sample_ < stts_atom_.total_samples) {
      return stsz_atom_.table[current_sample_];
    } else
      return 0;  // out of samples count
  } else {
    while (1) {
      if (trun_atom_.table_pointer < trun_atom_.table.size())
        return trun_atom_.table[trun_atom_.table_pointer].size;
      else if (is_fragmented_) {
        // read next fragment
        ret = fseek(file_, current_moof_.offset + current_moof_.size, SEEK_SET);
        if (ret) return 0;
        ret = parse_trun();
        if (ret) return 0;
        trun_atom_.table_pointer = 0;
      } else
        return 0;
    }
  }
}

AvxRational Mp4Parser::FrameRate() {
  AvxRational ret;
  if (is_qt_format_) {
    ret.numerator = (int)mdhd_atom_.timescale;
    ret.denominator = (int)stts_atom_.table[0].sample_delta;
  } else {
    ret.numerator = (int)mvhd_atom_.timescale;
    ret.denominator = (int)tfhd_atom_.defaultSampleDuration;
  }
  if (ret.numerator == 0 || ret.denominator == 0) {
    ret.numerator = 30;
    ret.denominator = 1;
  }
  return ret;
}

extern "C" mp4context* create_mp4_ctx() {
  mp4context* ctx = new mp4context;
  memset(ctx, 0, sizeof(mp4context));
  ctx->parser = new Mp4Parser();
  return ctx;
}

extern "C" int destroy_mp4_ctx(mp4context* ctx) {
  if (!ctx || !ctx->parser) return -1;
  Mp4Parser* parser = static_cast<Mp4Parser*>(ctx->parser);
  if (ctx->buffer) delete[] ctx->buffer;
  delete parser;
  delete ctx;
  return 0;
}

extern "C" int file_is_mp4(mp4context* ctx, AvxInputContext* input_ctx) {
  if (!ctx || !ctx->file || !ctx->parser) return 0;
  Mp4Parser* parser = static_cast<Mp4Parser*>(ctx->parser);
  long pos = ftell(ctx->file);
  fseek(ctx->file, 0, SEEK_SET);
  int ret = parser->OpenFile(ctx->file);
  if (!ret) {
    fseek(ctx->file, pos, SEEK_SET);
    if (input_ctx) input_ctx->framerate = parser->FrameRate();
    return 1;
  }
  return 0;
}

extern "C" int mp4_read_frame(mp4context* ctx, uint8_t** buf, size_t* bytes_in_buf, size_t* buffer_size) {
  if (!ctx || !ctx->file || !ctx->parser) return -1;
  Mp4Parser* parser = static_cast<Mp4Parser*>(ctx->parser);
  uint32_t buf_sz = parser->GetSize();
  if (!buf_sz) return -1;
  if (ctx->buffer_capacity < buf_sz) {
    if (ctx->buffer) delete[] ctx->buffer;
    ctx->buffer = new uint8_t[buf_sz];
    ctx->buffer_capacity = buf_sz;
  }
  int ret = parser->Read(ctx->buffer);
  if (ret) return -1;
  *buf = ctx->buffer;
  *bytes_in_buf = buf_sz;
  *buffer_size = ctx->buffer_capacity;
  return 0;
}