// SPDX-License-Identifier: BSD-2-Clause

// Prints the tables and inline functions of libaom's headers that the Rust host has copies of
// (crates/rav1g/src/host/tables.rs, blocks.rs), as text: the reference of testing/tables_check. The
// program tools/rav1g-tables prints the same lines from the copies; run.sh compares the two.
//
// A table is one line: "table <name> <type of a number> <count of numbers>: <numbers>", all dimensions
// in one row. A function is one line per input.

#include <stdint.h>
#include <stdio.h>
#include <string.h>

#include "aom_dsp/intrapred_common.h"
#include "av1/common/blockd.h"
#include "av1/common/common_data.h"
#include "av1/common/filter.h"
#include "av1/common/onyxc_int.h"
#include "av1/common/reconintra.h"

// One number of `size` bytes, signed or not.
static long long number(const unsigned char *at, size_t size, int is_signed) {
  if (size == 1) {
    uint8_t value;
    memcpy(&value, at, 1);
    return is_signed ? (long long)(int8_t)value : (long long)value;
  }
  if (size == 2) {
    int16_t value;
    memcpy(&value, at, 2);
    return is_signed ? (long long)value : (long long)(uint16_t)value;
  }
  int32_t value;
  memcpy(&value, at, 4);
  return is_signed ? (long long)value : (long long)(uint32_t)value;
}

static void table(const char *name, const void *data, size_t bytes, size_t size, int is_signed) {
  printf("table %s %c%zu %zu:", name, is_signed ? 'i' : 'u', size * 8, bytes / size);
  for (size_t at = 0; at + size <= bytes; at += size) {
    printf(" %lld", number((const unsigned char *)data + at, size, is_signed));
  }
  printf("\n");
}

// `type` is the type of one number of the table and `first` its first number; a table whose numbers
// are of another type does not compile.
#define TABLE(name, first, type)                                                                \
  do {                                                                                          \
    _Static_assert(sizeof(name) % sizeof(type) == 0, #name ": not a whole count of numbers");    \
    _Static_assert(_Generic(&(first), const type * : 1, default : 0),                           \
                   #name ": the numbers of the table are not of this type");                    \
    table(#name, name, sizeof(name), sizeof(type), (type)-1 < 0);                               \
  } while (0)
#define TABLE1(name, type) TABLE(name, name[0], type)
#define TABLE2(name, type) TABLE(name, name[0][0], type)
#define TABLE3(name, type) TABLE(name, name[0][0][0], type)

int main(void) {
  // av1/common/common_data.h
  TABLE1(block_size_wide, uint8_t);
  TABLE1(block_size_high, uint8_t);
  TABLE3(ss_size_lookup, BLOCK_SIZE);
  // ss_size_lookup[bsize][1][1]: the copy SS_SIZE_420 of the host.
  printf("table ss_size_lookup[][1][1] u8 %d:", BLOCK_SIZES_ALL);
  for (int bsize = 0; bsize < BLOCK_SIZES_ALL; bsize++) {
    printf(" %d", ss_size_lookup[bsize][1][1]);
  }
  printf("\n");
  TABLE1(tx_size_wide, int);
  TABLE1(tx_size_high, int);
  TABLE1(mi_size_wide_log2, uint8_t);
  TABLE1(mi_size_high_log2, uint8_t);
  TABLE1(max_txsize_rect_lookup, TX_SIZE);
  // av1/common/reconintra.h
  TABLE1(interintra_to_intra_mode, PREDICTION_MODE);
  // aom_dsp/intrapred_common.h
  TABLE1(sm_weight_arrays, uint8_t);
  // av1/common/common_data.h
  TABLE1(tx_size_wide_log2, int);
  TABLE1(tx_size_high_log2, int);
  // av1/common/blockd.h
  TABLE1(max_neighbor_obmc, int);
  // av1/common/common_data.h
  TABLE2(quant_dist_weight, int);
  TABLE3(quant_dist_lookup_table, int);
  // av1/common/filter.h
  TABLE2(av1_sub_pel_filters_4, int16_t);
  TABLE2(av1_sub_pel_filters_4smooth, int16_t);
  TABLE2(av1_bilinear_filters, int16_t);
  TABLE2(av1_sub_pel_filters_8, int16_t);
  TABLE2(av1_sub_pel_filters_8smooth, int16_t);
  TABLE2(av1_sub_pel_filters_8sharp, int16_t);

  // scale_chroma_bsize of av1/common/onyxc_int.h, for planes of half the width and height.
  for (int bsize = 0; bsize < BLOCK_SIZES_ALL; bsize++) {
    printf("scale_chroma_bsize(%d, 1, 1) = %d\n", bsize, (int)scale_chroma_bsize((BLOCK_SIZE)bsize, 1, 1));
  }
  // av1_get_max_uv_txsize of av1/common/blockd.h, for every subsampling a block size has a plane
  // block for.
  for (int bsize = 0; bsize < BLOCK_SIZES_ALL; bsize++) {
    for (int ss_x = 0; ss_x < 2; ss_x++) {
      for (int ss_y = 0; ss_y < 2; ss_y++) {
        if (ss_size_lookup[bsize][ss_x][ss_y] == BLOCK_INVALID) {
          printf("av1_get_max_uv_txsize(%d, %d, %d): no such plane block\n", bsize, ss_x, ss_y);
        } else {
          printf("av1_get_max_uv_txsize(%d, %d, %d) = %d\n", bsize, ss_x, ss_y,
                 (int)av1_get_max_uv_txsize((BLOCK_SIZE)bsize, ss_x, ss_y));
        }
      }
    }
  }
  // is_interintra_pred of av1/common/blockd.h.
  for (int bsize = 0; bsize < BLOCK_SIZES_ALL; bsize++) {
    for (int mode = 0; mode < 25; mode++) {
      for (int ref0 = -1; ref0 < 8; ref0++) {
        for (int ref1 = -1; ref1 < 8; ref1++) {
          MB_MODE_INFO mi;
          memset(&mi, 0, sizeof(mi));
          mi.sb_type = (BLOCK_SIZE)bsize;
          mi.mode = (PREDICTION_MODE)mode;
          mi.ref_frame[0] = (MV_REFERENCE_FRAME)ref0;
          mi.ref_frame[1] = (MV_REFERENCE_FRAME)ref1;
          printf("is_interintra_pred(%d, %d, %d, %d) = %d\n", bsize, mode, ref0, ref1, is_interintra_pred(&mi) != 0);
        }
      }
    }
  }
  // av1_extract_interp_filter of av1/common/filter.h: every pair of four bits, and bits that are not a
  // filter's.
  for (int y = 0; y < 16; y++) {
    for (int x = 0; x < 16; x++) {
      const uint32_t filters = ((uint32_t)x << 16) | (uint32_t)y;
      printf("av1_extract_interp_filter(0x%08x, 1) = %d\n", (unsigned)filters,
             (int)av1_extract_interp_filter(filters, 1));
      printf("av1_extract_interp_filter(0x%08x, 0) = %d\n", (unsigned)filters,
             (int)av1_extract_interp_filter(filters, 0));
    }
  }
  const uint32_t more[] = { 0xfff0fff0u, 0xffffffffu, 0x12345678u, 0x80000001u };
  for (size_t at = 0; at < sizeof(more) / sizeof(more[0]); at++) {
    printf("av1_extract_interp_filter(0x%08x, 1) = %d\n", (unsigned)more[at],
           (int)av1_extract_interp_filter(more[at], 1));
    printf("av1_extract_interp_filter(0x%08x, 0) = %d\n", (unsigned)more[at],
           (int)av1_extract_interp_filter(more[at], 0));
  }
  return 0;
}
