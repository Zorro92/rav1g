// Host check for the sequence header peek (libav1/av1/common/seq_header_peek.h), the code behind
// aom_codec_peek_stream_info() and the test player's -fitsize.
//
// Sequence headers are written here bit by bit from the syntax tables of the AV1 specification (5.5.1 to
// 5.5.5), independently of the parser, with known values; the peek must return those values. Covered: the
// header of an SVT-AV1 4K encode (the one -fitsize got wrong), timing info with and without decoder model
// info, several operating points with their own decoder model and display delay fields, frame size fields
// of other widths than 16 bits, the reduced still picture header, the three profiles and bit depths, and a
// few thousand random combinations. Every header is also cut short at every length: the peek must say so and
// must not read past the end. The headers are then wrapped into OBUs and found again with the decoder's own
// OBU header reader (av1/common/obu_util.c), the way decoder_peek_si_internal() walks a temporal unit.
//
// Build and run: sh testing/seq_header_check/run.sh

#include <cstdint>
#include <cstdio>
#include <cstring>
#include <vector>

#include "av1/common/obu_util.h"
#include "av1/common/seq_header_peek.h"

namespace {

int g_failures = 0;
int g_checks = 0;

void fail(const char *name, const char *what, long long expected, long long actual) {
  ++g_failures;
  if (g_failures <= 40) printf("MISMATCH %s: %s expected %lld, got %lld\n", name, what, expected, actual);
}

#define CHECK_EQ(name, what, expected, actual)                               \
  do {                                                                       \
    ++g_checks;                                                              \
    if ((long long)(expected) != (long long)(actual)) fail(name, what, (long long)(expected), (long long)(actual)); \
  } while (0)

struct BitWriter {
  std::vector<uint8_t> bytes;
  uint64_t bits = 0;
  void put(uint64_t value, int count) {
    for (int i = count - 1; i >= 0; --i) {
      if ((bits & 7) == 0) bytes.push_back(0);
      if ((value >> i) & 1) bytes.back() |= (uint8_t)(0x80 >> (bits & 7));
      ++bits;
    }
  }
  void uvlc(uint32_t value) {
    const uint64_t v = (uint64_t)value + 1;
    int length = 0;
    while ((v >> length) > 1) ++length;
    put(0, length);
    put(v, length + 1);
  }
};

struct OperatingPoint {
  int idc = 0;
  int level = 0;
  int tier = 0;
  int decoder_model_present = 0;
  uint32_t decoder_buffer_delay = 0;
  uint32_t encoder_buffer_delay = 0;
  int low_delay_mode = 0;
  int display_delay_present = 0;
  int display_delay_minus_1 = 0;
};

struct SeqHeader {
  int profile = 0;
  int still_picture = 0;
  int reduced = 0;
  int timing_info = 0;
  uint32_t num_units_in_display_tick = 1;
  uint32_t time_scale = 24;
  int equal_picture_interval = 0;
  uint32_t num_ticks_per_picture_minus_1 = 0;
  int decoder_model_info = 0;
  int buffer_delay_length_minus_1 = 0;
  uint32_t num_units_in_decoding_tick = 1;
  int buffer_removal_time_length_minus_1 = 0;
  int frame_presentation_time_length_minus_1 = 0;
  int display_delay_present = 0;
  std::vector<OperatingPoint> ops = std::vector<OperatingPoint>(1);
  int width_bits = 16;
  int height_bits = 16;
  int width = 1920;
  int height = 1080;
  int frame_id_numbers = 0;
  int delta_frame_id_length_minus_2 = 0;
  int additional_frame_id_length_minus_1 = 0;
  int sb128 = 0;
  int filter_intra = 1;
  int intra_edge_filter = 1;
  int interintra = 1;
  int masked = 1;
  int warped = 1;
  int dual_filter = 1;
  int order_hint = 1;
  int jnt_comp = 1;
  int ref_frame_mvs = 1;
  int choose_screen_content = 1;
  int force_screen_content = 0;
  int choose_integer_mv = 1;
  int force_integer_mv = 0;
  int order_hint_bits_minus_1 = 6;
  int superres = 0;
  int cdef = 1;
  int restoration = 1;
  int high_bitdepth = 0;
  int twelve_bit = 0;
  int monochrome = 0;
};

// sequence_header_obu() of the specification. Returns the bit position after mono_chrome (or after the bit
// depth fields in profile 1, which has no mono_chrome): the last bit the peek has to read.
uint64_t write_sequence_header(const SeqHeader &h, BitWriter *w) {
  w->put(h.profile, 3);
  w->put(h.still_picture, 1);
  w->put(h.reduced, 1);
  if (h.reduced) {
    w->put(h.ops[0].level, 5);
  } else {
    w->put(h.timing_info, 1);
    if (h.timing_info) {
      w->put(h.num_units_in_display_tick, 32);
      w->put(h.time_scale, 32);
      w->put(h.equal_picture_interval, 1);
      if (h.equal_picture_interval) w->uvlc(h.num_ticks_per_picture_minus_1);
      w->put(h.decoder_model_info, 1);
      if (h.decoder_model_info) {
        w->put(h.buffer_delay_length_minus_1, 5);
        w->put(h.num_units_in_decoding_tick, 32);
        w->put(h.buffer_removal_time_length_minus_1, 5);
        w->put(h.frame_presentation_time_length_minus_1, 5);
      }
    }
    w->put(h.display_delay_present, 1);
    w->put(h.ops.size() - 1, 5);
    for (const OperatingPoint &op : h.ops) {
      w->put(op.idc, 12);
      w->put(op.level, 5);
      if (op.level > 7) w->put(op.tier, 1);
      if (h.timing_info && h.decoder_model_info) {
        w->put(op.decoder_model_present, 1);
        if (op.decoder_model_present) {
          w->put(op.decoder_buffer_delay, h.buffer_delay_length_minus_1 + 1);
          w->put(op.encoder_buffer_delay, h.buffer_delay_length_minus_1 + 1);
          w->put(op.low_delay_mode, 1);
        }
      }
      if (h.display_delay_present) {
        w->put(op.display_delay_present, 1);
        if (op.display_delay_present) w->put(op.display_delay_minus_1, 4);
      }
    }
  }
  w->put(h.width_bits - 1, 4);
  w->put(h.height_bits - 1, 4);
  w->put(h.width - 1, h.width_bits);
  w->put(h.height - 1, h.height_bits);
  if (!h.reduced) {
    w->put(h.frame_id_numbers, 1);
    if (h.frame_id_numbers) {
      w->put(h.delta_frame_id_length_minus_2, 4);
      w->put(h.additional_frame_id_length_minus_1, 3);
    }
  }
  w->put(h.sb128, 1);
  w->put(h.filter_intra, 1);
  w->put(h.intra_edge_filter, 1);
  if (!h.reduced) {
    w->put(h.interintra, 1);
    w->put(h.masked, 1);
    w->put(h.warped, 1);
    w->put(h.dual_filter, 1);
    w->put(h.order_hint, 1);
    if (h.order_hint) {
      w->put(h.jnt_comp, 1);
      w->put(h.ref_frame_mvs, 1);
    }
    w->put(h.choose_screen_content, 1);
    if (!h.choose_screen_content) w->put(h.force_screen_content, 1);
    if (h.choose_screen_content || h.force_screen_content) {
      w->put(h.choose_integer_mv, 1);
      if (!h.choose_integer_mv) w->put(h.force_integer_mv, 1);
    }
    if (h.order_hint) w->put(h.order_hint_bits_minus_1, 3);
  }
  w->put(h.superres, 1);
  w->put(h.cdef, 1);
  w->put(h.restoration, 1);
  // color_config()
  w->put(h.high_bitdepth, 1);
  if (h.profile == 2 && h.high_bitdepth) w->put(h.twelve_bit, 1);
  if (h.profile != 1) w->put(h.monochrome, 1);
  const uint64_t peek_end = w->bits;
  w->put(0, 1);  // color_description_present_flag
  if (h.monochrome && h.profile != 1) {
    w->put(0, 1);  // color_range
  } else {
    w->put(0, 1);  // color_range
    if (h.profile == 2 && h.high_bitdepth && h.twelve_bit) w->put(1, 1), w->put(1, 1);  // subsampling_x, _y
    if (h.profile == 0 || (h.profile == 2 && h.high_bitdepth && h.twelve_bit)) w->put(0, 2);  // chroma_sample_position
    w->put(0, 1);  // separate_uv_delta_q
  }
  w->put(0, 1);  // film_grain_params_present
  w->put(1, 1);  // trailing_one_bit
  return peek_end;
}

int expected_bit_depth(const SeqHeader &h) {
  if (!h.high_bitdepth) return 8;
  return (h.profile == 2 && h.twelve_bit) ? 12 : 10;
}

// What the peek read before it was fixed: no timing_info_present_flag, no decoder model info and no
// initial_display_delay_present_flag. Kept here to show that the test header reproduces the console result.
void legacy_peek(const uint8_t *data, size_t size, int *width, int *height) {
  Av1SeqPeekBits rb = {data, size, 0, 0};
  av1_seq_peek_bits(&rb, 3);
  av1_seq_peek_bits(&rb, 1);
  const int reduced = (int)av1_seq_peek_bits(&rb, 1);
  if (reduced) {
    av1_seq_peek_bits(&rb, 5);
  } else {
    const int count = (int)av1_seq_peek_bits(&rb, 5) + 1;
    for (int i = 0; i < count; ++i) {
      av1_seq_peek_bits(&rb, 12);
      if (av1_seq_peek_bits(&rb, 5) > 7) av1_seq_peek_bits(&rb, 1);
    }
  }
  const int width_bits = (int)av1_seq_peek_bits(&rb, 4) + 1;
  const int height_bits = (int)av1_seq_peek_bits(&rb, 4) + 1;
  *width = (int)av1_seq_peek_bits(&rb, width_bits) + 1;
  *height = (int)av1_seq_peek_bits(&rb, height_bits) + 1;
}

void check_header(const char *name, const SeqHeader &h) {
  BitWriter w;
  const uint64_t peek_end = write_sequence_header(h, &w);
  const std::vector<uint8_t> &bytes = w.bytes;
  Av1SeqHeaderPeek out;
  const int status = av1_peek_sequence_header(bytes.data(), bytes.size(), &out);
  CHECK_EQ(name, "status", AV1_SEQ_PEEK_OK, status);
  if (status != AV1_SEQ_PEEK_OK) return;
  CHECK_EQ(name, "profile", h.profile, out.profile);
  CHECK_EQ(name, "still_picture", h.still_picture, out.still_picture);
  CHECK_EQ(name, "reduced_still_picture_header", h.reduced, out.reduced_still_picture_header);
  CHECK_EQ(name, "timing_info_present", h.reduced ? 0 : h.timing_info, out.timing_info_present);
  CHECK_EQ(name, "decoder_model_info_present", (!h.reduced && h.timing_info) ? h.decoder_model_info : 0,
           out.decoder_model_info_present);
  CHECK_EQ(name, "initial_display_delay_present", h.reduced ? 0 : h.display_delay_present,
           out.initial_display_delay_present);
  CHECK_EQ(name, "operating_points_cnt", h.reduced ? 1 : (int)h.ops.size(), out.operating_points_cnt);
  CHECK_EQ(name, "operating_point_idc0", h.reduced ? 0 : h.ops[0].idc, out.operating_point_idc0);
  CHECK_EQ(name, "seq_level_idx0", h.ops[0].level, out.seq_level_idx0);
  CHECK_EQ(name, "seq_tier0", (!h.reduced && h.ops[0].level > 7) ? h.ops[0].tier : 0, out.seq_tier0);
  CHECK_EQ(name, "frame_width_bits", h.width_bits, out.frame_width_bits);
  CHECK_EQ(name, "frame_height_bits", h.height_bits, out.frame_height_bits);
  CHECK_EQ(name, "max_frame_width", h.width, out.max_frame_width);
  CHECK_EQ(name, "max_frame_height", h.height, out.max_frame_height);
  CHECK_EQ(name, "use_128x128_superblock", h.sb128, out.use_128x128_superblock);
  CHECK_EQ(name, "force_screen_content_tools",
           (h.reduced || h.choose_screen_content) ? 2 : h.force_screen_content, out.force_screen_content_tools);
  CHECK_EQ(name, "enable_superres", h.superres, out.enable_superres);
  CHECK_EQ(name, "enable_cdef", h.cdef, out.enable_cdef);
  CHECK_EQ(name, "enable_restoration", h.restoration, out.enable_restoration);
  CHECK_EQ(name, "bit_depth", expected_bit_depth(h), out.bit_depth);
  CHECK_EQ(name, "monochrome", h.profile == 1 ? 0 : h.monochrome, out.monochrome);
  CHECK_EQ(name, "bits_read", peek_end, out.bits_read);

  // Cut short: every length that ends before the last bit the peek needs must be reported, and the peek must
  // not look at a byte past the length it was given (the copy has no byte there to look at).
  const size_t needed = (size_t)((peek_end + 7) / 8);
  for (size_t length = 0; length <= bytes.size(); ++length) {
    std::vector<uint8_t> cut(bytes.begin(), bytes.begin() + length);
    Av1SeqHeaderPeek part;
    const int s = av1_peek_sequence_header(cut.data(), cut.size(), &part);
    CHECK_EQ(name, length < needed ? "status of a cut header" : "status of a long enough header",
             length < needed ? AV1_SEQ_PEEK_TRUNCATED : AV1_SEQ_PEEK_OK, s);
    if (length >= needed && s == AV1_SEQ_PEEK_OK) {
      CHECK_EQ(name, "max_frame_width of a long enough header", h.width, part.max_frame_width);
      CHECK_EQ(name, "max_frame_height of a long enough header", h.height, part.max_frame_height);
      CHECK_EQ(name, "bit_depth of a long enough header", expected_bit_depth(h), part.bit_depth);
    }
  }
}

// One OBU: header, optional extension byte, leb128 payload size (|size_bytes| long), payload.
void append_obu(std::vector<uint8_t> *out, int type, const std::vector<uint8_t> &payload, int extension,
                int size_bytes) {
  out->push_back((uint8_t)((type << 3) | (extension ? 4 : 0) | 2));  // has_size_field
  if (extension) out->push_back((uint8_t)((1 << 5) | (1 << 3)));     // temporal_id 1, spatial_id 1
  size_t size = payload.size();
  for (int i = 0; i < size_bytes; ++i) {
    uint8_t byte = (uint8_t)(size & 0x7f);
    size >>= 7;
    if (i + 1 < size_bytes) byte |= 0x80;
    out->push_back(byte);
  }
  out->insert(out->end(), payload.begin(), payload.end());
}

// The OBU walk of decoder_peek_si_internal(): skip the temporal delimiter, then every OBU until a sequence
// header was read and a key frame header was seen. Returns 1 when both were found.
int peek_temporal_unit(const std::vector<uint8_t> &tu, Av1SeqHeaderPeek *out, int *status) {
  const uint8_t *data = tu.data();
  size_t data_sz = tu.size();
  int got_sequence_header = 0;
  *status = AV1_SEQ_PEEK_TRUNCATED;
  while (data_sz > 0) {
    ObuHeader obu_header;
    memset(&obu_header, 0, sizeof(obu_header));
    size_t payload_size = 0;
    size_t bytes_read = 0;
    if (aom_read_obu_header_and_size(data, data_sz, 0, &obu_header, &payload_size, &bytes_read) != AOM_CODEC_OK)
      return 0;
    data += bytes_read;
    data_sz -= bytes_read;
    if (data_sz < payload_size) return 0;
    if (obu_header.type == OBU_SEQUENCE_HEADER) {
      *status = av1_peek_sequence_header(data, payload_size, out);
      if (*status != AV1_SEQ_PEEK_OK) return 0;
      got_sequence_header = 1;
    } else if (obu_header.type == OBU_FRAME_HEADER || obu_header.type == OBU_FRAME) {
      // show_existing_frame 0, frame_type KEY_FRAME (0)
      if (got_sequence_header && payload_size > 0 && (data[0] & 0xe0) == 0) return 1;
    }
    data += payload_size;
    data_sz -= payload_size;
  }
  return 0;
}

void check_temporal_unit(const char *name, const SeqHeader &h, int extension, int size_bytes) {
  BitWriter w;
  write_sequence_header(h, &w);
  if (w.bytes.size() > 127 && size_bytes < 2) size_bytes = 2;  // one size byte holds up to 127
  std::vector<uint8_t> tu;
  append_obu(&tu, OBU_TEMPORAL_DELIMITER, std::vector<uint8_t>(), 0, 1);
  append_obu(&tu, OBU_SEQUENCE_HEADER, w.bytes, extension, size_bytes);
  // Bytes that would read as a huge frame if the peek ran on into the next OBU.
  append_obu(&tu, OBU_FRAME, std::vector<uint8_t>(24, 0x10), extension, 1);
  Av1SeqHeaderPeek out;
  int status = 0;
  const int found = peek_temporal_unit(tu, &out, &status);
  CHECK_EQ(name, "sequence header and key frame found in the temporal unit", 1, found);
  if (!found) return;
  CHECK_EQ(name, "max_frame_width in the temporal unit", h.width, out.max_frame_width);
  CHECK_EQ(name, "max_frame_height in the temporal unit", h.height, out.max_frame_height);
  CHECK_EQ(name, "bit_depth in the temporal unit", expected_bit_depth(h), out.bit_depth);

  // A sequence header OBU that ends too early is reported even when more OBUs follow it.
  std::vector<uint8_t> short_payload(w.bytes.begin(), w.bytes.begin() + 3);
  std::vector<uint8_t> cut;
  append_obu(&cut, OBU_TEMPORAL_DELIMITER, std::vector<uint8_t>(), 0, 1);
  append_obu(&cut, OBU_SEQUENCE_HEADER, short_payload, extension, size_bytes);
  append_obu(&cut, OBU_FRAME, std::vector<uint8_t>(24, 0xff), extension, 1);
  CHECK_EQ(name, "a cut sequence header OBU is not accepted", 0, peek_temporal_unit(cut, &out, &status));
  CHECK_EQ(name, "status of a cut sequence header OBU", AV1_SEQ_PEEK_TRUNCATED, status);
}

uint32_t g_rng = 0x1234567u;
uint32_t rnd(uint32_t n) {
  g_rng ^= g_rng << 13;
  g_rng ^= g_rng >> 17;
  g_rng ^= g_rng << 5;
  return n ? g_rng % n : 0;
}

SeqHeader random_header() {
  SeqHeader h;
  h.profile = (int)rnd(3);
  h.still_picture = (int)rnd(4) == 0;
  h.reduced = h.still_picture && rnd(2);
  h.timing_info = (int)rnd(2);
  h.num_units_in_display_tick = 1 + rnd(0xfffffffe);
  h.time_scale = 1 + rnd(0xfffffffe);
  h.equal_picture_interval = (int)rnd(2);
  h.num_ticks_per_picture_minus_1 = rnd(4) ? rnd(1000) : rnd(0xfffffffe);
  h.decoder_model_info = (int)rnd(2);
  h.buffer_delay_length_minus_1 = (int)rnd(32);
  h.num_units_in_decoding_tick = 1 + rnd(0xfffffffe);
  h.buffer_removal_time_length_minus_1 = (int)rnd(32);
  h.frame_presentation_time_length_minus_1 = (int)rnd(32);
  h.display_delay_present = (int)rnd(2);
  h.ops.resize(h.reduced ? 1 : 1 + rnd(rnd(3) ? 4 : 32));
  for (OperatingPoint &op : h.ops) {
    op.idc = (int)rnd(4096);
    op.level = (int)rnd(32);
    op.tier = (int)rnd(2);
    op.decoder_model_present = (int)rnd(2);
    const int n = h.buffer_delay_length_minus_1 + 1;
    op.decoder_buffer_delay = n >= 32 ? rnd(0xffffffffu) : rnd(1u << n);
    op.encoder_buffer_delay = n >= 32 ? rnd(0xffffffffu) : rnd(1u << n);
    op.low_delay_mode = (int)rnd(2);
    op.display_delay_present = (int)rnd(2);
    op.display_delay_minus_1 = (int)rnd(16);
  }
  h.width_bits = 1 + (int)rnd(16);
  h.height_bits = 1 + (int)rnd(16);
  h.width = 1 + (int)rnd(1u << h.width_bits);
  h.height = 1 + (int)rnd(1u << h.height_bits);
  h.frame_id_numbers = (int)rnd(2);
  h.delta_frame_id_length_minus_2 = (int)rnd(16);
  h.additional_frame_id_length_minus_1 = (int)rnd(8);
  h.sb128 = (int)rnd(2);
  h.filter_intra = (int)rnd(2);
  h.intra_edge_filter = (int)rnd(2);
  h.interintra = (int)rnd(2);
  h.masked = (int)rnd(2);
  h.warped = (int)rnd(2);
  h.dual_filter = (int)rnd(2);
  h.order_hint = (int)rnd(2);
  h.jnt_comp = (int)rnd(2);
  h.ref_frame_mvs = (int)rnd(2);
  h.choose_screen_content = (int)rnd(2);
  h.force_screen_content = (int)rnd(2);
  h.choose_integer_mv = (int)rnd(2);
  h.force_integer_mv = (int)rnd(2);
  h.order_hint_bits_minus_1 = (int)rnd(8);
  h.superres = (int)rnd(2);
  h.cdef = (int)rnd(2);
  h.restoration = (int)rnd(2);
  h.high_bitdepth = (int)rnd(2);
  h.twelve_bit = (int)rnd(2);
  h.monochrome = (int)rnd(2);
  return h;
}

}  // namespace

int main() {
  // The sequence header of an SVT-AV1 4K 10-bit encode: main profile, no timing info, one operating point at
  // level 5.1 (seq_level_idx 13) with a tier bit, 12-bit frame size fields, 3840x2160.
  SeqHeader svt;
  svt.ops[0].level = 13;
  svt.width_bits = 12;
  svt.height_bits = 12;
  svt.width = 3840;
  svt.height = 2160;
  svt.high_bitdepth = 1;
  svt.restoration = 0;
  check_header("svt 3840x2160 10-bit", svt);
  {
    // The console logged "the stream's largest frame is 32x128" for this stream. The old reader gives exactly
    // that for this header: it starts the operating points two bits early.
    BitWriter w;
    write_sequence_header(svt, &w);
    int width = 0, height = 0;
    legacy_peek(w.bytes.data(), w.bytes.size(), &width, &height);
    CHECK_EQ("svt 3840x2160 10-bit", "width the reader before the fix returned", 32, width);
    CHECK_EQ("svt 3840x2160 10-bit", "height the reader before the fix returned", 128, height);
  }

  SeqHeader timing = svt;
  timing.timing_info = 1;
  timing.num_units_in_display_tick = 1001;
  timing.time_scale = 24000;
  check_header("timing info, no equal picture interval", timing);
  timing.equal_picture_interval = 1;
  timing.num_ticks_per_picture_minus_1 = 0;
  check_header("timing info, equal picture interval, uvlc 0", timing);
  timing.num_ticks_per_picture_minus_1 = 1000;
  check_header("timing info, equal picture interval, uvlc 1000", timing);
  timing.num_ticks_per_picture_minus_1 = 0xfffffffeu;
  check_header("timing info, equal picture interval, longest uvlc", timing);

  SeqHeader model = timing;
  model.num_ticks_per_picture_minus_1 = 1;
  model.decoder_model_info = 1;
  model.buffer_delay_length_minus_1 = 19;
  model.num_units_in_decoding_tick = 90000;
  model.buffer_removal_time_length_minus_1 = 9;
  model.frame_presentation_time_length_minus_1 = 31;
  model.ops[0].decoder_model_present = 1;
  model.ops[0].decoder_buffer_delay = 70000;
  model.ops[0].encoder_buffer_delay = 20000;
  check_header("decoder model info, one operating point", model);
  model.ops[0].decoder_model_present = 0;
  check_header("decoder model info, not for the operating point", model);
  model.buffer_delay_length_minus_1 = 31;
  model.ops[0].decoder_model_present = 1;
  model.ops[0].decoder_buffer_delay = 0xfedcba98u;
  model.ops[0].encoder_buffer_delay = 0x89abcdefu;
  model.ops[0].low_delay_mode = 1;
  check_header("decoder model info, 32-bit buffer delays", model);

  SeqHeader layers = model;
  layers.buffer_delay_length_minus_1 = 9;
  layers.display_delay_present = 1;
  layers.ops.resize(3);
  layers.ops[0].idc = 0x301;
  layers.ops[0].level = 12;
  layers.ops[0].tier = 1;
  layers.ops[0].decoder_model_present = 1;
  layers.ops[0].decoder_buffer_delay = 700;
  layers.ops[0].encoder_buffer_delay = 200;
  layers.ops[0].display_delay_present = 1;
  layers.ops[0].display_delay_minus_1 = 9;
  layers.ops[1].idc = 0x101;
  layers.ops[1].level = 7;  // no tier bit
  layers.ops[1].decoder_model_present = 0;
  layers.ops[1].display_delay_present = 0;
  layers.ops[2].idc = 0x103;
  layers.ops[2].level = 8;
  layers.ops[2].decoder_model_present = 1;
  layers.ops[2].decoder_buffer_delay = 1;
  layers.ops[2].encoder_buffer_delay = 1023;
  layers.ops[2].display_delay_present = 1;
  layers.ops[2].display_delay_minus_1 = 3;
  layers.width_bits = 13;
  layers.height_bits = 12;
  layers.width = 4096;
  layers.height = 2176;
  check_header("three operating points, decoder model and display delay", layers);
  layers.timing_info = 0;
  check_header("three operating points, display delay only", layers);
  layers.ops.resize(32);
  for (size_t i = 3; i < layers.ops.size(); ++i) layers.ops[i].level = (int)i;
  check_header("32 operating points", layers);

  SeqHeader hd;
  hd.ops[0].level = 8;
  hd.width_bits = 11;
  hd.height_bits = 11;
  hd.width = 1920;
  hd.height = 1080;
  check_header("1920x1080 8-bit, 11-bit size fields", hd);
  hd.width_bits = 16;
  hd.height_bits = 16;
  hd.width = 65536;
  hd.height = 65536;
  check_header("65536x65536, 16-bit size fields", hd);
  hd.width_bits = 1;
  hd.height_bits = 1;
  hd.width = 2;
  hd.height = 1;
  check_header("2x1, 1-bit size fields", hd);
  hd.width_bits = 9;
  hd.height_bits = 5;
  hd.width = 352;
  hd.height = 32;
  hd.frame_id_numbers = 1;
  hd.delta_frame_id_length_minus_2 = 13;
  hd.additional_frame_id_length_minus_1 = 2;
  hd.order_hint = 0;
  hd.choose_screen_content = 0;
  hd.force_screen_content = 0;
  check_header("frame id numbers, no order hint, screen content tools off", hd);
  hd.force_screen_content = 1;
  hd.choose_integer_mv = 0;
  hd.force_integer_mv = 1;
  hd.sb128 = 1;
  check_header("screen content tools forced on, integer mv forced", hd);

  SeqHeader still;
  still.still_picture = 1;
  still.reduced = 1;
  still.ops[0].level = 9;
  still.width_bits = 10;
  still.height_bits = 9;
  still.width = 1000;
  still.height = 500;
  check_header("reduced still picture header", still);
  still.reduced = 0;
  check_header("still picture, full header", still);

  SeqHeader high = svt;
  high.profile = 1;  // 4:4:4, no mono_chrome bit
  check_header("profile 1, 10-bit", high);
  high.high_bitdepth = 0;
  check_header("profile 1, 8-bit", high);
  high.profile = 2;
  high.high_bitdepth = 1;
  high.twelve_bit = 1;
  check_header("profile 2, 12-bit", high);
  high.twelve_bit = 0;
  high.monochrome = 1;
  check_header("profile 2, 10-bit, monochrome", high);

  // A video sequence with the reduced header is not a legal stream; the decoder refuses it too.
  {
    SeqHeader bad = still;
    bad.still_picture = 0;
    bad.reduced = 1;
    BitWriter w;
    write_sequence_header(bad, &w);
    Av1SeqHeaderPeek out;
    CHECK_EQ("reduced header without still_picture", "status", AV1_SEQ_PEEK_UNSUPPORTED,
             av1_peek_sequence_header(w.bytes.data(), w.bytes.size(), &out));
    CHECK_EQ("no data", "status", AV1_SEQ_PEEK_TRUNCATED, av1_peek_sequence_header(NULL, 16, &out));
  }

  // Inside OBUs, found with the decoder's OBU header reader: plain, with an extension header, and with a
  // payload size written in two and in three bytes.
  check_temporal_unit("svt in a temporal unit", svt, 0, 1);
  check_temporal_unit("svt in a temporal unit, extension header", svt, 1, 1);
  check_temporal_unit("svt in a temporal unit, 2-byte size", svt, 0, 2);
  check_temporal_unit("layers in a temporal unit, 2-byte size", layers, 1, 2);
  check_temporal_unit("layers in a temporal unit, 3-byte size", layers, 0, 3);
  check_temporal_unit("reduced still picture header in a temporal unit", still, 0, 1);

  const int random_headers = 4000;
  for (int i = 0; i < random_headers; ++i) {
    char name[48];
    snprintf(name, sizeof(name), "random header %d", i);
    const SeqHeader h = random_header();
    check_header(name, h);
    if ((i & 15) == 0) check_temporal_unit(name, h, (int)rnd(2), 1 + (int)rnd(3));
  }

  if (g_failures) {
    printf("seq_header_check: FAILED, %d of %d checks\n", g_failures, g_checks);
    return 1;
  }
  printf("seq_header_check: OK, %d checks on hand-written and %d random sequence headers\n", g_checks,
         random_headers);
  return 0;
}
