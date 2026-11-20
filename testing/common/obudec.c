/*
 * Copyright (c) 2017, Alliance for Open Media. All rights reserved
 *
 * This source code is subject to the terms of the BSD 2 Clause License and
 * the Alliance for Open Media Patent License 1.0. If the BSD 2 Clause License
 * was not distributed with this source code in the LICENSE file, you can
 * obtain it at www.aomedia.org/license/software. If the Alliance for Open
 * Media Patent License 1.0 was not distributed with this source code in the
 * PATENTS file, you can obtain it at www.aomedia.org/license/patent.
 */

#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include "common/obudec.h"

#include "aom_dsp/aom_dsp_common.h"
#include "aom_ports/mem_ops.h"
#include "av1/common/common.h"
#include "av1/common/obu_util.h"

#define OBU_BUFFER_SIZE (500 * 1024)

#define OBU_HEADER_SIZE 1
#define OBU_EXTENSION_SIZE 1
#define OBU_MAX_LENGTH_FIELD_SIZE 8

#define OBU_MAX_HEADER_SIZE (OBU_HEADER_SIZE + OBU_EXTENSION_SIZE + OBU_MAX_LENGTH_FIELD_SIZE)

// aom_uleb_decode() of aom/src/aom_integer.c and aom_read_obu_header() of av1/common/obu_util.c, here
// because the programs that read streams with this file no longer link the C of libav1 (the decoder's
// parser is rav1d's parse pass, docs/phase2-rav1d.md): the reader stands by itself.
static int obudec_uleb_decode(const uint8_t *buffer, size_t available, uint64_t *value, size_t *length) {
  if (buffer && value) {
    *value = 0;
    for (size_t i = 0; i < OBU_MAX_LENGTH_FIELD_SIZE && i < available; ++i) {
      const uint8_t decoded_byte = *(buffer + i) & 0x7f;
      *value |= ((uint64_t)decoded_byte) << (i * 7);
      if ((*(buffer + i) >> 7) == 0) {
        if (length) *length = i + 1;
        // Values above 32 bits fail, as they do in libaom.
        if (*value > UINT32_MAX) return -1;
        return 0;
      }
    }
  }
  // The pointers were invalid, or the value does not end in the bytes there are.
  return -1;
}

static aom_codec_err_t obudec_read_obu_header_bits(uint8_t *buffer, size_t buffer_length, size_t *consumed,
                                                   ObuHeader *header) {
  if (buffer_length < 1 || !consumed || !header) return AOM_CODEC_INVALID_PARAM;
  const uint8_t first = buffer[0];
  header->size = 1;
  // The forbidden bit must not be set.
  if (first & 0x80) return AOM_CODEC_CORRUPT_FRAME;
  header->type = (OBU_TYPE)((first >> 3) & 15);
  switch (header->type) {
    case OBU_SEQUENCE_HEADER:
    case OBU_TEMPORAL_DELIMITER:
    case OBU_FRAME_HEADER:
    case OBU_TILE_GROUP:
    case OBU_METADATA:
    case OBU_FRAME:
    case OBU_REDUNDANT_FRAME_HEADER:
    case OBU_TILE_LIST:
    case OBU_PADDING: break;
    default: return AOM_CODEC_CORRUPT_FRAME;
  }
  header->has_extension = (first >> 2) & 1;
  header->has_size_field = (first >> 1) & 1;
  // Section 5 streams must have the size field.
  if (!header->has_size_field) return AOM_CODEC_UNSUP_BITSTREAM;
  // obu_reserved_1bit must be 0.
  if (first & 1) return AOM_CODEC_CORRUPT_FRAME;
  if (header->has_extension) {
    if (buffer_length == 1) return AOM_CODEC_CORRUPT_FRAME;
    header->size += 1;
    header->temporal_layer_id = buffer[1] >> 5;
    header->spatial_layer_id = (buffer[1] >> 3) & 3;
    // extension_header_reserved_3bits must be 0.
    if (buffer[1] & 7) return AOM_CODEC_CORRUPT_FRAME;
  } else {
    header->temporal_layer_id = 0;
    header->spatial_layer_id = 0;
  }
  *consumed = header->size;
  return AOM_CODEC_OK;
}

// Reads unsigned LEB128 integer and returns 0 upon successful read and decode.
// Stores raw bytes in 'value_buffer', length of the number in 'value_length',
// and decoded value in 'value'.
static int obudec_read_leb128(FILE *f, uint8_t *value_buffer, size_t *value_length, uint64_t *value) {
  if (!f || !value_buffer || !value_length || !value) return -1;
  size_t len;
  for (len = 0; len < OBU_MAX_LENGTH_FIELD_SIZE; ++len) {
    const size_t num_read = fread(&value_buffer[len], 1, 1, f);
    if (num_read == 0) {
      if (len == 0 && feof(f)) {
        *value_length = 0;
        return 0;
      }
      // Ran out of data before completing read of value.
      return -1;
    }
    if ((value_buffer[len] >> 7) == 0) {
      ++len;
      *value_length = len;
      break;
    }
  }

  return obudec_uleb_decode(value_buffer, len, value, NULL);
}

// Reads OBU header from 'f'. The 'buffer_capacity' passed in must be large
// enough to store an OBU header with extension (2 bytes). Raw OBU data is
// written to 'obu_data', parsed OBU header values are written to 'obu_header',
// and total bytes read from file are written to 'bytes_read'. Returns 0 for
// success, and non-zero on failure. When end of file is reached, the return
// value is 0 and the 'bytes_read' value is set to 0.
static int obudec_read_obu_header(FILE *f, size_t buffer_capacity, uint8_t *obu_data,
                                  ObuHeader *obu_header, size_t *bytes_read) {
  if (!f || buffer_capacity < (OBU_HEADER_SIZE + OBU_EXTENSION_SIZE) || !obu_data || !obu_header || !bytes_read) {
    return -1;
  }
  *bytes_read = fread(obu_data, 1, 1, f);

  if (feof(f) && *bytes_read == 0) {
    return 0;
  } else if (*bytes_read != 1) {
    fprintf(stderr, "obudec: Failure reading OBU header.\n");
    return -1;
  }

  const int has_extension = (obu_data[0] >> 2) & 0x1;
  if (has_extension) {
    if (fread(&obu_data[1], 1, 1, f) != 1) {
      fprintf(stderr, "obudec: Failure reading OBU extension.");
      return -1;
    }
    ++*bytes_read;
  }

  size_t obu_bytes_parsed = 0;
  const aom_codec_err_t parse_result =
      obudec_read_obu_header_bits(obu_data, *bytes_read, &obu_bytes_parsed, obu_header);
  if (parse_result != AOM_CODEC_OK || *bytes_read != obu_bytes_parsed) {
    // fprintf(stderr, "obudec: Error parsing OBU header.\n");
    return -1;
  }

  return 0;
}

// Reads OBU payload from 'f' and returns 0 for success when all payload bytes
// are read from the file. Payload data is written to 'obu_data', and actual
// bytes read added to 'bytes_read'.
static int obudec_read_obu_payload(FILE *f, size_t payload_length, uint8_t *obu_data, size_t *bytes_read) {
  if (!f || payload_length == 0 || !obu_data || !bytes_read) return -1;

  if (fread(obu_data, 1, payload_length, f) != payload_length) {
    fprintf(stderr, "obudec: Failure reading OBU payload.\n");
    return -1;
  }

  *bytes_read += payload_length;
  return 0;
}

static int obudec_read_obu_header_and_size(FILE *f, size_t buffer_capacity, uint8_t *buffer,
                                           size_t *bytes_read, size_t *payload_length, ObuHeader *obu_header) {
  size_t header_size = 0;
  if (obudec_read_obu_header(f, buffer_capacity, buffer, obu_header, &header_size) != 0) return -1;
  if (header_size == 0) return 0;

  size_t size_length = 0;
  uint64_t payload_size;
  if (obudec_read_leb128(f, buffer + header_size, &size_length, &payload_size) != 0 || size_length == 0) {
    fprintf(stderr, "obudec: Failure reading OBU payload length.\n");
    return -1;
  }

  *payload_length = (size_t)payload_size;
  *bytes_read = header_size + size_length;
  return 0;
}

static int obudec_grow_buffer(size_t growth_amount, uint8_t **obu_buffer, size_t *obu_buffer_capacity) {
  if (!*obu_buffer || !obu_buffer_capacity || growth_amount == 0) {
    return -1;
  }

  const size_t capacity = *obu_buffer_capacity;
  if (SIZE_MAX - growth_amount < capacity) {
    fprintf(stderr, "obudec: cannot grow buffer, capacity will roll over.\n");
    return -1;
  }

  const size_t new_capacity = capacity + growth_amount;

#if defined AOM_MAX_ALLOCABLE_MEMORY
  if (new_capacity > AOM_MAX_ALLOCABLE_MEMORY) {
    fprintf(stderr, "obudec: OBU size exceeds max alloc size.\n");
    return -1;
  }
#endif

  uint8_t *new_buffer = (uint8_t *)realloc(*obu_buffer, new_capacity);
  if (!new_buffer) {
    fprintf(stderr, "obudec: Failed to allocate compressed data buffer.\n");
    return -1;
  }

  *obu_buffer = new_buffer;
  *obu_buffer_capacity = new_capacity;
  return 0;
}

static int obudec_read_one_obu(FILE *f, uint8_t **obu_buffer, size_t obu_bytes_buffered, size_t *obu_buffer_capacity,
                               size_t *obu_length, ObuHeader *obu_header) {
  if (!f || !(*obu_buffer) || !obu_buffer_capacity || !obu_length || !obu_header) {
    return -1;
  }

  size_t bytes_read = 0;
  size_t obu_payload_length = 0;
  size_t available_buffer_capacity = *obu_buffer_capacity - obu_bytes_buffered;

  if (available_buffer_capacity < OBU_MAX_HEADER_SIZE) {
    if (obudec_grow_buffer(AOMMAX(*obu_buffer_capacity, OBU_MAX_HEADER_SIZE), obu_buffer, obu_buffer_capacity) != 0) {
      return -2;
    }
    available_buffer_capacity = *obu_buffer_capacity - obu_bytes_buffered;
  }

  const int status =
      obudec_read_obu_header_and_size(f, available_buffer_capacity, *obu_buffer + obu_bytes_buffered,
                                      &bytes_read, &obu_payload_length, obu_header);
  if (status < 0) return status;

  if (obu_payload_length > SIZE_MAX - bytes_read) return -1;

  if (obu_payload_length > 256 * 1024 * 1024) {
    fprintf(stderr, "obudec: Read invalid OBU size (%u)\n", (unsigned int)obu_payload_length);
    *obu_length = bytes_read + obu_payload_length;
    return -1;
  }

  if (bytes_read + obu_payload_length > available_buffer_capacity &&
      obudec_grow_buffer(AOMMAX(*obu_buffer_capacity, bytes_read + obu_payload_length - available_buffer_capacity),
                         obu_buffer, obu_buffer_capacity) != 0) {
    return -2;
  }

  if (obu_payload_length > 0 &&
      obudec_read_obu_payload(f, obu_payload_length, *obu_buffer + obu_bytes_buffered + bytes_read, &bytes_read) != 0) {
    return -1;
  }

  *obu_length = bytes_read;
  return 0;
}

int file_is_obu(struct ObuDecInputContext *obu_ctx) {
  FILE *f = obu_ctx->avx_ctx->file;
  uint8_t detect_buf[OBU_MAX_HEADER_SIZE];
  size_t bytes_read = 0;
  size_t payload_length = 0;
  ObuHeader header;

  if (obudec_read_obu_header_and_size(f, sizeof(detect_buf), detect_buf, &bytes_read, &payload_length, &header) != 0 ||
      bytes_read == 0 ||
      (header.type != OBU_TEMPORAL_DELIMITER && header.type != OBU_SEQUENCE_HEADER) ||
      (header.type == OBU_TEMPORAL_DELIMITER && payload_length != 0) ||
      payload_length > OBU_BUFFER_SIZE - bytes_read) {
    rewind(f);
    return 0;
  }

  obu_ctx->buffer = (uint8_t *)malloc(OBU_BUFFER_SIZE);
  if (!obu_ctx->buffer) {
    fprintf(stderr, "obudec: Out of memory.\n");
    rewind(f);
    return 0;
  }
  obu_ctx->buffer_capacity = OBU_BUFFER_SIZE;
  memcpy(obu_ctx->buffer, detect_buf, bytes_read);
  if (payload_length > 0 &&
      obudec_read_obu_payload(f, payload_length, obu_ctx->buffer + bytes_read, &bytes_read) != 0) {
    obudec_free(obu_ctx);
    rewind(f);
    return 0;
  }
  obu_ctx->bytes_buffered = bytes_read;
  return 1;
}

static int obudec_reserve(uint8_t **buffer, size_t *capacity, size_t size) {
  if (size <= *capacity) return 0;
  uint8_t *new_buffer = (uint8_t *)realloc(*buffer, size);
  if (!new_buffer) {
    fprintf(stderr, "obudec: Out of memory.\n");
    return -2;
  }
  *buffer = new_buffer;
  *capacity = size;
  return 0;
}

// Read an Annex B length bounded by its parent. Return the size after consuming the length itself.
static int obudec_annexb_size(const uint8_t **input, const uint8_t *end, size_t *size) {
  uint64_t value;
  size_t length;
  if (obudec_uleb_decode(*input, end - *input, &value, &length) != 0) return -1;
  *input += length;
  if (value > (uint64_t)(end - *input)) return -1;
  *size = (size_t)value;
  return 0;
}

// Strip the temporal, frame and OBU lengths. Replace each outer OBU length with an inner payload
// size when absent, so that the decoder can consume the temporal unit as low-overhead OBUs.
static int obudec_normalize_annexb(uint8_t *buffer, size_t size, size_t *bytes_read) {
  const uint8_t *input = buffer;
  const uint8_t *end = buffer + size;
  uint8_t *output = buffer;
  while (input < end) {
    size_t frame_size;
    if (obudec_annexb_size(&input, end, &frame_size) != 0) return -1;
    const uint8_t *frame_end = input + frame_size;
    while (input < frame_end) {
      size_t obu_size;
      if (obudec_annexb_size(&input, frame_end, &obu_size) != 0 || obu_size == 0) return -1;
      const uint8_t *obu_end = input + obu_size;
      const size_t header_size = OBU_HEADER_SIZE + ((*input & 4) ? OBU_EXTENSION_SIZE : 0);
      if (obu_size < header_size) return -1;
      if (*input & 2) {
        const uint8_t *payload = input + header_size;
        size_t payload_size;
        if (obudec_annexb_size(&payload, obu_end, &payload_size) != 0 ||
            payload_size != (size_t)(obu_end - payload)) return -1;
        memmove(output, input, obu_size);
        output += obu_size;
      } else {
        // The payload size takes no more bytes than the outer OBU length did, so compact in place.
        memmove(output, input, header_size);
        *output |= 2;
        output += header_size;
        size_t remaining = obu_size - header_size;
        do {
          uint8_t byte = remaining & 0x7f;
          remaining >>= 7;
          *output++ = byte | (remaining ? 0x80 : 0);
        } while (remaining);
        memmove(output, input + header_size, obu_size - header_size);
        output += obu_size - header_size;
      }
      input = obu_end;
    }
  }
  *bytes_read = output - buffer;
  return *bytes_read == 0 ? -1 : 0;
}

static int obudec_read_annexb(FILE *f, uint8_t **buffer, size_t *bytes_read, size_t *capacity) {
  uint8_t encoded[OBU_MAX_LENGTH_FIELD_SIZE];
  size_t length = 0;
  uint64_t value;
  if (obudec_read_leb128(f, encoded, &length, &value) != 0) return ferror(f) ? -2 : -1;
  if (length == 0) return 1;
  const size_t size = (size_t)value;
  if (size == 0) return -1;

  // Grow as bytes arrive. A truncated file can advertise a much larger unit than it contains.
  size_t offset = 0;
  while (offset < size) {
    if (offset == *capacity) {
      const size_t next = *capacity ? *capacity + AOMMIN(*capacity, size - *capacity) : AOMMIN(size, OBU_BUFFER_SIZE);
      if (obudec_reserve(buffer, capacity, next) != 0) return -2;
    }
    const size_t chunk = AOMMIN(size - offset, *capacity - offset);
    if (fread(*buffer + offset, 1, chunk, f) != chunk) return ferror(f) ? -2 : -1;
    offset += chunk;
  }
  return obudec_normalize_annexb(*buffer, size, bytes_read);
}

int obudec_read_temporal_unit(struct ObuDecInputContext *obu_ctx, uint8_t **buffer, size_t *bytes_read,
                              size_t *buffer_size) {
  FILE *f = obu_ctx->avx_ctx->file;
  if (!f) return 1;

  *bytes_read = 0;
  if (obu_ctx->is_annexb) {
    const int status = obudec_read_annexb(f, buffer, bytes_read, buffer_size);
    if (status == -1) fprintf(stderr, "obudec: Invalid or truncated Annex B temporal unit.\n");
    return status;
  }

  if (feof(f)) return 1;
  size_t obu_size;
  while (1) {
    ObuHeader header = {0};
    const int status = obudec_read_one_obu(f, &obu_ctx->buffer, obu_ctx->bytes_buffered, &obu_ctx->buffer_capacity,
                                            &obu_size, &header);
    if (status != 0) return ferror(f) ? -2 : status;
    if (header.type == OBU_TEMPORAL_DELIMITER || obu_size == 0) break;
    obu_ctx->bytes_buffered += obu_size;
  }

  const size_t size = obu_ctx->bytes_buffered;
  if (size == 0) return 1;
  if (size > UINT32_MAX) return -1;
  if (obudec_reserve(buffer, buffer_size, size) != 0) return -2;
  memcpy(*buffer, obu_ctx->buffer, size);
  memmove(obu_ctx->buffer, obu_ctx->buffer + size, obu_size);
  obu_ctx->bytes_buffered = obu_size;
  *bytes_read = size;
  return 0;
}

void obudec_free(struct ObuDecInputContext *obu_ctx) {
  free(obu_ctx->buffer);
  obu_ctx->buffer = NULL;
}
