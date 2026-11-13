/*
 * Copyright (c) 2026, Alliance for Open Media. All rights reserved
 *
 * This source code is subject to the terms of the BSD 2 Clause License and
 * the Alliance for Open Media Patent License 1.0. If the BSD 2 Clause License
 * was not distributed with this source code in the LICENSE file, you can
 * obtain it at www.aomedia.org/license/software. If the Alliance for Open
 * Media Patent License 1.0 was not distributed with this source code in the
 * PATENTS file, you can obtain it at www.aomedia.org/license/patent.
 */

/*
 * Reads the fields of a sequence header OBU payload that an application needs
 * before a decoder exists: the largest frame size, the bit depth and the
 * colour description.
 *
 * The decoder's own parser (read_sequence_header_obu in av1/decoder/obu.c)
 * cannot be used for this: it writes into an AV1Decoder, reports errors with
 * longjmp and reads past the end of a truncated buffer. This one walks the
 * same syntax (AV1 specification 5.5, in the order of obu.c,
 * av1_read_sequence_header and av1_read_color_config), keeps nothing but the
 * fields below and stops at the end of the data.
 *
 * Plain C without dependencies, so it also compiles on a host without the
 * decoder (testing/seq_header_check).
 */

#ifndef AOM_AV1_COMMON_SEQ_HEADER_PEEK_H_
#define AOM_AV1_COMMON_SEQ_HEADER_PEEK_H_

#include <stddef.h>
#include <stdint.h>
#include <string.h>

#ifdef __cplusplus
extern "C" {
#endif

typedef struct Av1SeqHeaderPeek {
  int profile;
  int still_picture;
  int reduced_still_picture_header;
  int timing_info_present;
  int decoder_model_info_present;
  int initial_display_delay_present;
  int operating_points_cnt;  /* operating_points_cnt_minus_1 + 1 */
  int operating_point_idc0;  /* operating_point_idc[0] */
  int seq_level_idx0;        /* seq_level_idx[0] */
  int seq_tier0;             /* seq_tier[0] */
  int frame_width_bits;      /* frame_width_bits_minus_1 + 1 */
  int frame_height_bits;     /* frame_height_bits_minus_1 + 1 */
  int max_frame_width;       /* max_frame_width_minus_1 + 1 */
  int max_frame_height;      /* max_frame_height_minus_1 + 1 */
  int use_128x128_superblock;
  int force_screen_content_tools; /* 0, 1, or 2 when frames choose */
  int enable_superres;
  int enable_cdef;
  int enable_restoration;
  int bit_depth;             /* 8, 10 or 12 */
  int monochrome;
  /* The rest of color_config(): the colour description. The three code points
     are 2 (unspecified) without color_description_present_flag. */
  int color_description_present;
  int color_primaries;
  int transfer_characteristics;
  int matrix_coefficients;
  int color_range;           /* 1: full range */
  int subsampling_x;
  int subsampling_y;
  int chroma_sample_position;
  uint32_t bits_read;        /* up to and including the last field above */
} Av1SeqHeaderPeek;

enum {
  AV1_SEQ_PEEK_OK = 0,
  AV1_SEQ_PEEK_TRUNCATED = -1,   /* the data ends inside the fields above */
  AV1_SEQ_PEEK_UNSUPPORTED = -2  /* reduced_still_picture_header without still_picture */
};

typedef struct Av1SeqPeekBits {
  const uint8_t *data;
  size_t size;
  uint64_t pos; /* in bits */
  int failed;   /* a read went past the end; every read after it returns 0 */
} Av1SeqPeekBits;

static uint32_t av1_seq_peek_bits(Av1SeqPeekBits *rb, int count) {
  uint32_t value = 0;
  int i;
  for (i = 0; i < count; ++i) {
    const uint64_t byte = rb->pos >> 3;
    if (rb->failed || byte >= rb->size) {
      rb->failed = 1;
      return 0;
    }
    value = (value << 1) | (uint32_t)((rb->data[byte] >> (7 - (int)(rb->pos & 7))) & 1);
    ++rb->pos;
  }
  return value;
}

/* uvlc(): the value is not needed, only its length. */
static void av1_seq_peek_skip_uvlc(Av1SeqPeekBits *rb) {
  int leading_zeros = 0;
  while (!rb->failed && !av1_seq_peek_bits(rb, 1)) {
    if (++leading_zeros >= 32) return; /* 32 zeros: the value is (1 << 32) - 1, nothing follows */
  }
  av1_seq_peek_bits(rb, leading_zeros);
}

/* |data| is the payload of a sequence header OBU (after the OBU header and its
 * size field), |size| the bytes available. Returns AV1_SEQ_PEEK_OK and fills
 * |out|, or one of the errors above. Values are not checked for being legal:
 * the decoder does that when it reads the header. */
static int av1_peek_sequence_header(const uint8_t *data, size_t size, Av1SeqHeaderPeek *out) {
  Av1SeqPeekBits bits;
  Av1SeqPeekBits *const rb = &bits;
  int i;
  bits.data = data;
  bits.size = data ? size : 0;
  bits.pos = 0;
  bits.failed = 0;
  memset(out, 0, sizeof(*out));

  out->profile = (int)av1_seq_peek_bits(rb, 3);
  out->still_picture = (int)av1_seq_peek_bits(rb, 1);
  out->reduced_still_picture_header = (int)av1_seq_peek_bits(rb, 1);
  if (rb->failed) return AV1_SEQ_PEEK_TRUNCATED;
  if (!out->still_picture && out->reduced_still_picture_header) return AV1_SEQ_PEEK_UNSUPPORTED;

  if (out->reduced_still_picture_header) {
    out->operating_points_cnt = 1;
    out->seq_level_idx0 = (int)av1_seq_peek_bits(rb, 5);
  } else {
    int buffer_delay_length = 0;
    out->timing_info_present = (int)av1_seq_peek_bits(rb, 1);
    if (out->timing_info_present) {
      /* timing_info() */
      av1_seq_peek_bits(rb, 32); /* num_units_in_display_tick */
      av1_seq_peek_bits(rb, 32); /* time_scale */
      if (av1_seq_peek_bits(rb, 1)) av1_seq_peek_skip_uvlc(rb); /* equal_picture_interval: num_ticks_per_picture_minus_1 */
      out->decoder_model_info_present = (int)av1_seq_peek_bits(rb, 1);
      if (out->decoder_model_info_present) {
        /* decoder_model_info() */
        buffer_delay_length = (int)av1_seq_peek_bits(rb, 5) + 1;
        av1_seq_peek_bits(rb, 32); /* num_units_in_decoding_tick */
        av1_seq_peek_bits(rb, 5);  /* buffer_removal_time_length_minus_1 */
        av1_seq_peek_bits(rb, 5);  /* frame_presentation_time_length_minus_1 */
      }
    }
    out->initial_display_delay_present = (int)av1_seq_peek_bits(rb, 1);
    out->operating_points_cnt = (int)av1_seq_peek_bits(rb, 5) + 1;
    for (i = 0; i < out->operating_points_cnt && !rb->failed; ++i) {
      const int idc = (int)av1_seq_peek_bits(rb, 12);
      const int level = (int)av1_seq_peek_bits(rb, 5);
      const int tier = level > 7 ? (int)av1_seq_peek_bits(rb, 1) : 0;
      if (i == 0) {
        out->operating_point_idc0 = idc;
        out->seq_level_idx0 = level;
        out->seq_tier0 = tier;
      }
      if (out->decoder_model_info_present) {
        if (av1_seq_peek_bits(rb, 1)) { /* decoder_model_present_for_this_op */
          /* operating_parameters_info() */
          av1_seq_peek_bits(rb, buffer_delay_length); /* decoder_buffer_delay */
          av1_seq_peek_bits(rb, buffer_delay_length); /* encoder_buffer_delay */
          av1_seq_peek_bits(rb, 1);                   /* low_delay_mode_flag */
        }
      }
      if (out->initial_display_delay_present) {
        if (av1_seq_peek_bits(rb, 1)) av1_seq_peek_bits(rb, 4); /* initial_display_delay_minus_1 */
      }
    }
  }

  out->frame_width_bits = (int)av1_seq_peek_bits(rb, 4) + 1;
  out->frame_height_bits = (int)av1_seq_peek_bits(rb, 4) + 1;
  out->max_frame_width = (int)av1_seq_peek_bits(rb, out->frame_width_bits) + 1;
  out->max_frame_height = (int)av1_seq_peek_bits(rb, out->frame_height_bits) + 1;

  if (!out->reduced_still_picture_header) {
    if (av1_seq_peek_bits(rb, 1)) { /* frame_id_numbers_present_flag */
      av1_seq_peek_bits(rb, 4);     /* delta_frame_id_length_minus_2 */
      av1_seq_peek_bits(rb, 3);     /* additional_frame_id_length_minus_1 */
    }
  }
  out->use_128x128_superblock = (int)av1_seq_peek_bits(rb, 1);
  av1_seq_peek_bits(rb, 1); /* enable_filter_intra */
  av1_seq_peek_bits(rb, 1); /* enable_intra_edge_filter */
  if (out->reduced_still_picture_header) {
    out->force_screen_content_tools = 2;
  } else {
    int enable_order_hint;
    av1_seq_peek_bits(rb, 1); /* enable_interintra_compound */
    av1_seq_peek_bits(rb, 1); /* enable_masked_compound */
    av1_seq_peek_bits(rb, 1); /* enable_warped_motion */
    av1_seq_peek_bits(rb, 1); /* enable_dual_filter */
    enable_order_hint = (int)av1_seq_peek_bits(rb, 1);
    if (enable_order_hint) {
      av1_seq_peek_bits(rb, 1); /* enable_jnt_comp */
      av1_seq_peek_bits(rb, 1); /* enable_ref_frame_mvs */
    }
    if (av1_seq_peek_bits(rb, 1)) /* seq_choose_screen_content_tools */
      out->force_screen_content_tools = 2;
    else
      out->force_screen_content_tools = (int)av1_seq_peek_bits(rb, 1);
    if (out->force_screen_content_tools > 0) {
      if (!av1_seq_peek_bits(rb, 1)) av1_seq_peek_bits(rb, 1); /* seq_choose_integer_mv, seq_force_integer_mv */
    }
    if (enable_order_hint) av1_seq_peek_bits(rb, 3); /* order_hint_bits_minus_1 */
  }
  out->enable_superres = (int)av1_seq_peek_bits(rb, 1);
  out->enable_cdef = (int)av1_seq_peek_bits(rb, 1);
  out->enable_restoration = (int)av1_seq_peek_bits(rb, 1);

  /* color_config(), its first fields */
  out->bit_depth = 8;
  if (av1_seq_peek_bits(rb, 1)) { /* high_bitdepth */
    out->bit_depth = 10;
    if (out->profile == 2 && av1_seq_peek_bits(rb, 1)) out->bit_depth = 12; /* twelve_bit */
  }
  out->monochrome = out->profile == 1 ? 0 : (int)av1_seq_peek_bits(rb, 1); /* mono_chrome */

  /* color_config(), the colour description */
  out->color_description_present = (int)av1_seq_peek_bits(rb, 1);
  out->color_primaries = out->transfer_characteristics = out->matrix_coefficients = 2; /* unspecified */
  if (out->color_description_present) {
    out->color_primaries = (int)av1_seq_peek_bits(rb, 8);
    out->transfer_characteristics = (int)av1_seq_peek_bits(rb, 8);
    out->matrix_coefficients = (int)av1_seq_peek_bits(rb, 8);
  }
  if (out->monochrome) {
    out->color_range = (int)av1_seq_peek_bits(rb, 1);
    out->subsampling_x = out->subsampling_y = 1;
  } else if (out->color_primaries == 1 && out->transfer_characteristics == 13 &&
             out->matrix_coefficients == 0) {
    /* BT.709 primaries, the sRGB transfer and the identity matrix: RGB, full range, 4:4:4 */
    out->color_range = 1;
  } else {
    out->color_range = (int)av1_seq_peek_bits(rb, 1);
    if (out->profile == 0) {
      out->subsampling_x = out->subsampling_y = 1;
    } else if (out->profile == 2) {
      if (out->bit_depth == 12) {
        out->subsampling_x = (int)av1_seq_peek_bits(rb, 1);
        if (out->subsampling_x) out->subsampling_y = (int)av1_seq_peek_bits(rb, 1);
      } else {
        out->subsampling_x = 1;
      }
    }
    if (out->subsampling_x && out->subsampling_y)
      out->chroma_sample_position = (int)av1_seq_peek_bits(rb, 2);
  }

  if (rb->failed) return AV1_SEQ_PEEK_TRUNCATED;
  out->bits_read = (uint32_t)rb->pos;
  return AV1_SEQ_PEEK_OK;
}

#ifdef __cplusplus
} /* extern "C" */
#endif

#endif /* AOM_AV1_COMMON_SEQ_HEADER_PEEK_H_ */
