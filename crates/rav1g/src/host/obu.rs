// SPDX-License-Identifier: BSD-2-Clause
//
// This source code is subject to the terms of the BSD 2 Clause License and the Alliance for Open Media
// Patent License 1.0. If the BSD 2 Clause License was not distributed with this source code in the LICENSE
// file, you can obtain it at www.aomedia.org/license/software. If the Alliance for Open Media Patent
// License 1.0 was not distributed with this source code in the PATENTS file, you can obtain it at
// www.aomedia.org/license/patent.

//! What the decoder reads of a temporal unit itself, next to the parser: the OBU headers, for the frame of
//! a temporal unit that is output, and the sequence header, for the stream info an application asks for
//! before it has a decoder.
//!
//! These are the functions of the C interface the decoder had while the entropy decoder of libaom was its
//! parser (`aom_read_obu_header_and_size` of `av1/common/obu_util.c`, `av1_frame_is_output` of
//! `av1/decoder/obu.c`, `decoder_peek_si_internal` of `av1/av1_dx_iface.c`, `av1_peek_sequence_header` of
//! `av1/common/seq_header_peek.h`), in Rust, with the same answers and the same errors. Streams are
//! section 5 streams: every OBU has its size field (no Annex B).

use std::ffi::c_int;

use crate::sys;
use crate::sys::parser::AV1Decoder;

const OBU_SEQUENCE_HEADER: u8 = 1;
const OBU_TEMPORAL_DELIMITER: u8 = 2;
const OBU_FRAME_HEADER: u8 = 3;
const OBU_TILE_GROUP: u8 = 4;
const OBU_METADATA: u8 = 5;
const OBU_FRAME: u8 = 6;
const OBU_REDUNDANT_FRAME_HEADER: u8 = 7;
const OBU_TILE_LIST: u8 = 8;
const OBU_PADDING: u8 = 15;

/// The header of an OBU and its size field.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct ObuHeader {
    pub kind: u8,
    pub temporal_layer_id: u8,
    pub spatial_layer_id: u8,
    /// The bytes of the payload behind the header and the size field.
    pub payload_size: usize,
    /// The bytes of the header and the size field.
    pub bytes_read: usize,
}

/// `aom_uleb_decode`: a value of at most 32 bits in at most 8 bytes, and the bytes it took.
fn uleb(data: &[u8]) -> Option<(u64, usize)> {
    let mut value = 0u64;
    for (index, &byte) in data.iter().take(8).enumerate() {
        value |= u64::from(byte & 0x7f) << (index * 7);
        if byte >> 7 == 0 {
            return (value <= u64::from(u32::MAX)).then_some((value, index + 1));
        }
    }
    None
}

/// `aom_read_obu_header_and_size` for a stream that is not Annex B: the OBU at the start of `data`, or
/// the error of the interface (a forbidden bit, a type that does not exist, a reserved bit: corrupt; no
/// size field: not supported).
pub(crate) fn read_obu_header(data: &[u8]) -> Result<ObuHeader, sys::aom_codec_err_t> {
    let Some(&first) = data.first() else {
        return Err(sys::AOM_CODEC_CORRUPT_FRAME);
    };
    if first & 0x80 != 0 {
        return Err(sys::AOM_CODEC_CORRUPT_FRAME);
    }
    let kind = (first >> 3) & 15;
    let valid = matches!(
        kind,
        OBU_SEQUENCE_HEADER
            | OBU_TEMPORAL_DELIMITER
            | OBU_FRAME_HEADER
            | OBU_TILE_GROUP
            | OBU_METADATA
            | OBU_FRAME
            | OBU_REDUNDANT_FRAME_HEADER
            | OBU_TILE_LIST
            | OBU_PADDING
    );
    if !valid {
        return Err(sys::AOM_CODEC_CORRUPT_FRAME);
    }
    let has_extension = first & 4 != 0;
    if first & 2 == 0 {
        return Err(sys::AOM_CODEC_UNSUP_BITSTREAM);
    }
    if first & 1 != 0 {
        return Err(sys::AOM_CODEC_CORRUPT_FRAME);
    }
    let mut header = ObuHeader { kind, ..Default::default() };
    let mut size = 1;
    if has_extension {
        let Some(&extension) = data.get(1) else {
            return Err(sys::AOM_CODEC_CORRUPT_FRAME);
        };
        size = 2;
        header.temporal_layer_id = extension >> 5;
        header.spatial_layer_id = (extension >> 3) & 3;
        if extension & 7 != 0 {
            return Err(sys::AOM_CODEC_CORRUPT_FRAME);
        }
    }
    let (payload, length) = uleb(&data[size..]).ok_or(sys::AOM_CODEC_CORRUPT_FRAME)?;
    header.payload_size = payload as usize;
    header.bytes_read = size + length;
    Ok(header)
}

/// `is_obu_in_current_operating_point` of `av1/decoder/obu.c`.
fn in_operating_point(operating_point_idc: c_int, header: &ObuHeader) -> bool {
    operating_point_idc == 0
        || ((operating_point_idc >> header.temporal_layer_id) & 1 != 0
            && (operating_point_idc >> (header.spatial_layer_id + 8)) & 1 != 0)
}

/// The rule of [`frame_is_output`] on the OBUs that follow a frame in the data of its decode call.
fn output_before(following: &[u8], spatial_layer_id: c_int, operating_point_idc: c_int) -> bool {
    let mut data = following;
    while !data.is_empty() {
        // Data that cannot be read as an OBU (the zero bytes allowed behind a frame, a damaged stream)
        // ends the search: nothing known follows.
        let Ok(header) = read_obu_header(data) else {
            return true;
        };
        if data.len() - header.bytes_read < header.payload_size || header.kind == OBU_TEMPORAL_DELIMITER {
            return true;
        }
        if matches!(header.kind, OBU_FRAME | OBU_FRAME_HEADER | OBU_REDUNDANT_FRAME_HEADER | OBU_TILE_GROUP)
            && c_int::from(header.spatial_layer_id) > spatial_layer_id
            && in_operating_point(operating_point_idc, &header)
        {
            return false;
        }
        data = &data[header.bytes_read + header.payload_size..];
    }
    true
}

/// The output process of the specification (section 7.18.1) for a stream with spatial layers
/// (`av1_frame_is_output` of `av1/decoder/obu.c`). Every layer that has a frame in a temporal unit has
/// one shown frame there, and an application that displays the video shows one frame of the temporal
/// unit: the frame of the highest spatial layer that is in the operating point and present in the
/// temporal unit. That is what libaom's decoder returns by default, and what the md5 files of AOM's
/// spatial layer vectors hold.
///
/// The frame's output is recorded with its command list, before the next frame of the temporal unit is
/// read, so the answer is taken from the OBU headers that follow the frame: a shown frame is output
/// unless an OBU of a frame of a higher spatial layer that is in the operating point follows in the
/// same temporal unit (up to the next temporal delimiter, or the end of the data of the decode call). A
/// stream without layers has no such OBU, and each of its shown frames is output.
///
/// # Safety
/// `pbi` is the frame's header with `next_obu` and `obus_end` null or the bounds of valid data.
pub(crate) unsafe fn frame_is_output(pbi: *const AV1Decoder) -> bool {
    // SAFETY: the caller's contract.
    unsafe {
        let pbi = &*pbi;
        if pbi.output_all_layers != 0 || pbi.next_obu.is_null() || pbi.obus_end.is_null() {
            return true;
        }
        let length = (pbi.obus_end as usize).saturating_sub(pbi.next_obu as usize);
        let following = std::slice::from_raw_parts(pbi.next_obu, length);
        output_before(following, pbi.common.spatial_layer_id, pbi.current_operating_point)
    }
}

/// Bits of a header, the most significant first. A read behind the end fails the reader and gives 0.
struct Bits<'a> {
    data: &'a [u8],
    position: usize,
    failed: bool,
}

impl Bits<'_> {
    fn read(&mut self, count: u32) -> u32 {
        let mut value = 0;
        for _ in 0..count {
            let Some(&byte) = self.data.get(self.position >> 3).filter(|_| !self.failed) else {
                self.failed = true;
                return 0;
            };
            value = (value << 1) | u32::from((byte >> (7 - (self.position & 7))) & 1);
            self.position += 1;
        }
        value
    }

    fn flag(&mut self) -> bool {
        self.read(1) != 0
    }

    fn skip_uvlc(&mut self) {
        let mut leading_zeros = 0;
        while !self.failed && !self.flag() {
            leading_zeros += 1;
            // 32 zeros: the value is (1 << 32) - 1, nothing follows.
            if leading_zeros >= 32 {
                return;
            }
        }
        self.read(leading_zeros);
    }
}

/// What the peek reads of a sequence header.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct SequencePeek {
    reduced_still_picture_header: bool,
    operating_point_idc0: u32,
    max_frame_width: u32,
    max_frame_height: u32,
    bit_depth: u32,
    monochrome: bool,
    color_description_present: bool,
    color_primaries: u32,
    transfer_characteristics: u32,
    matrix_coefficients: u32,
    color_range: u32,
    chroma_sample_position: u32,
}

/// `av1_peek_sequence_header`: the fields of a sequence header up to the end of its colour description.
/// The fields in front of the frame size (timing info, decoder model info, operating points) have a
/// variable length and are walked as the parser walks them. The error is that of the peek: a
/// `reduced_still_picture_header` without `still_picture` is not supported, a header that ends inside
/// these fields is corrupt.
fn peek_sequence_header(data: &[u8]) -> Result<SequencePeek, sys::aom_codec_err_t> {
    let mut bits = Bits { data, position: 0, failed: false };
    let rb = &mut bits;
    let mut out = SequencePeek::default();

    let profile = rb.read(3);
    let still_picture = rb.flag();
    out.reduced_still_picture_header = rb.flag();
    if rb.failed {
        return Err(sys::AOM_CODEC_CORRUPT_FRAME);
    }
    if !still_picture && out.reduced_still_picture_header {
        return Err(sys::AOM_CODEC_UNSUP_BITSTREAM);
    }

    if out.reduced_still_picture_header {
        rb.read(5); // seq_level_idx[0]
    } else {
        let mut buffer_delay_length = 0;
        let mut decoder_model_info_present = false;
        if rb.flag() {
            // timing_info()
            rb.read(32); // num_units_in_display_tick
            rb.read(32); // time_scale
            if rb.flag() {
                rb.skip_uvlc(); // equal_picture_interval: num_ticks_per_picture_minus_1
            }
            decoder_model_info_present = rb.flag();
            if decoder_model_info_present {
                // decoder_model_info()
                buffer_delay_length = rb.read(5) + 1;
                rb.read(32); // num_units_in_decoding_tick
                rb.read(5); // buffer_removal_time_length_minus_1
                rb.read(5); // frame_presentation_time_length_minus_1
            }
        }
        let initial_display_delay_present = rb.flag();
        let operating_points = rb.read(5) + 1;
        for point in 0..operating_points {
            if rb.failed {
                break;
            }
            let idc = rb.read(12);
            let level = rb.read(5);
            if level > 7 {
                rb.read(1); // seq_tier
            }
            if point == 0 {
                out.operating_point_idc0 = idc;
            }
            if decoder_model_info_present && rb.flag() {
                // operating_parameters_info()
                rb.read(buffer_delay_length); // decoder_buffer_delay
                rb.read(buffer_delay_length); // encoder_buffer_delay
                rb.read(1); // low_delay_mode_flag
            }
            if initial_display_delay_present && rb.flag() {
                rb.read(4); // initial_display_delay_minus_1
            }
        }
    }

    let frame_width_bits = rb.read(4) + 1;
    let frame_height_bits = rb.read(4) + 1;
    out.max_frame_width = rb.read(frame_width_bits) + 1;
    out.max_frame_height = rb.read(frame_height_bits) + 1;

    if !out.reduced_still_picture_header && rb.flag() {
        // frame_id_numbers_present_flag
        rb.read(4); // delta_frame_id_length_minus_2
        rb.read(3); // additional_frame_id_length_minus_1
    }
    rb.read(1); // use_128x128_superblock
    rb.read(1); // enable_filter_intra
    rb.read(1); // enable_intra_edge_filter
    if !out.reduced_still_picture_header {
        rb.read(1); // enable_interintra_compound
        rb.read(1); // enable_masked_compound
        rb.read(1); // enable_warped_motion
        rb.read(1); // enable_dual_filter
        let enable_order_hint = rb.flag();
        if enable_order_hint {
            rb.read(1); // enable_jnt_comp
            rb.read(1); // enable_ref_frame_mvs
        }
        // seq_choose_screen_content_tools, else seq_force_screen_content_tools
        let force_screen_content_tools = if rb.flag() { 2 } else { rb.read(1) };
        if force_screen_content_tools > 0 && !rb.flag() {
            rb.read(1); // seq_choose_integer_mv, seq_force_integer_mv
        }
        if enable_order_hint {
            rb.read(3); // order_hint_bits_minus_1
        }
    }
    rb.read(1); // enable_superres
    rb.read(1); // enable_cdef
    rb.read(1); // enable_restoration

    // color_config()
    out.bit_depth = 8;
    if rb.flag() {
        // high_bitdepth
        out.bit_depth = 10;
        if profile == 2 && rb.flag() {
            out.bit_depth = 12; // twelve_bit
        }
    }
    out.monochrome = profile != 1 && rb.flag();
    out.color_description_present = rb.flag();
    // Unspecified without a description.
    (out.color_primaries, out.transfer_characteristics, out.matrix_coefficients) = (2, 2, 2);
    if out.color_description_present {
        out.color_primaries = rb.read(8);
        out.transfer_characteristics = rb.read(8);
        out.matrix_coefficients = rb.read(8);
    }
    if out.monochrome {
        out.color_range = rb.read(1);
    } else if out.color_primaries == 1 && out.transfer_characteristics == 13 && out.matrix_coefficients == 0 {
        // BT.709 primaries, the sRGB transfer and the identity matrix: RGB, full range, 4:4:4.
        out.color_range = 1;
    } else {
        out.color_range = rb.read(1);
        let (mut subsampling_x, mut subsampling_y) = (false, false);
        if profile == 0 {
            (subsampling_x, subsampling_y) = (true, true);
        } else if profile == 2 {
            if out.bit_depth == 12 {
                subsampling_x = rb.flag();
                if subsampling_x {
                    subsampling_y = rb.flag();
                }
            } else {
                subsampling_x = true;
            }
        }
        if subsampling_x && subsampling_y {
            out.chroma_sample_position = rb.read(2);
        }
    }

    if rb.failed {
        return Err(sys::AOM_CODEC_CORRUPT_FRAME);
    }
    Ok(out)
}

/// What a temporal unit says about its stream (`aom_codec_stream_info_t`): the frame size, the bit depth
/// and the colour description of its sequence header, and whether it starts a decode (a sequence header
/// and a key frame).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct StreamPeek {
    pub width: u32,
    pub height: u32,
    pub is_key_frame: bool,
    pub bit_depth: u32,
    pub monochrome: bool,
    pub color_description_present: bool,
    pub color_primaries: u32,
    pub transfer_characteristics: u32,
    pub matrix_coefficients: u32,
    pub color_range: u32,
    pub chroma_sample_position: u32,
}

/// `aom_codec_peek_stream_info` of the decoder (`decoder_peek_si_internal`): reads the OBUs of `data` up
/// to the first frame header, without a decoder.
pub(crate) fn peek_stream_info(data: &[u8]) -> Result<StreamPeek, sys::aom_codec_err_t> {
    if data.is_empty() {
        return Err(sys::AOM_CODEC_INVALID_PARAM);
    }
    let mut info =
        StreamPeek { color_primaries: 2, transfer_characteristics: 2, matrix_coefficients: 2, ..Default::default() };
    let mut data = data;
    let mut header = read_obu_header(data)?;
    // A temporal delimiter at the start is skipped, with a payload it should not have.
    if header.kind == OBU_TEMPORAL_DELIMITER {
        if data.len() < header.bytes_read + header.payload_size {
            return Err(sys::AOM_CODEC_CORRUPT_FRAME);
        }
        data = &data[header.bytes_read + header.payload_size..];
        header = read_obu_header(data)?;
    }
    let mut got_sequence_header = false;
    let mut reduced_still_picture_header = false;
    let mut found_key_frame = false;
    loop {
        data = &data[header.bytes_read..];
        if data.len() < header.payload_size {
            return Err(sys::AOM_CODEC_CORRUPT_FRAME);
        }
        if header.kind == OBU_SEQUENCE_HEADER {
            if data.len() < 2 {
                return Err(sys::AOM_CODEC_CORRUPT_FRAME);
            }
            let sequence = peek_sequence_header(&data[..header.payload_size])?;
            reduced_still_picture_header = sequence.reduced_still_picture_header;
            info.width = sequence.max_frame_width;
            info.height = sequence.max_frame_height;
            info.bit_depth = sequence.bit_depth;
            info.monochrome = sequence.monochrome;
            info.color_description_present = sequence.color_description_present;
            info.color_primaries = sequence.color_primaries;
            info.transfer_characteristics = sequence.transfer_characteristics;
            info.matrix_coefficients = sequence.matrix_coefficients;
            info.color_range = sequence.color_range;
            info.chroma_sample_position = sequence.chroma_sample_position;
            got_sequence_header = true;
        } else if header.kind == OBU_FRAME_HEADER || header.kind == OBU_FRAME {
            if got_sequence_header && reduced_still_picture_header {
                found_key_frame = true;
                break;
            }
            // Enough bits for the frame type; bits behind the data read as 0, as the interface read them.
            if data.is_empty() {
                return Err(sys::AOM_CODEC_CORRUPT_FRAME);
            }
            let mut bits = Bits { data, position: 0, failed: false };
            let show_existing_frame = bits.flag();
            // frame_type 0 is a key frame.
            if !show_existing_frame && bits.read(2) == 0 {
                found_key_frame = true;
                // No further OBU changes the outcome.
                break;
            }
        }
        data = &data[header.payload_size..];
        if data.is_empty() {
            break;
        }
        header = read_obu_header(data)?;
    }
    info.is_key_frame = got_sequence_header && found_key_frame;
    Ok(info)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_header_of_an_obu() {
        // A temporal delimiter with a size field of 0; a frame header OBU with an extension and 3 bytes.
        assert_eq!(
            read_obu_header(&[0x12, 0x00, 0xff]),
            Ok(ObuHeader { kind: OBU_TEMPORAL_DELIMITER, bytes_read: 2, ..Default::default() })
        );
        assert_eq!(
            read_obu_header(&[0x1e, 0x28, 0x03, 1, 2, 3, 9]),
            Ok(ObuHeader {
                kind: OBU_FRAME_HEADER,
                temporal_layer_id: 1,
                spatial_layer_id: 1,
                payload_size: 3,
                bytes_read: 3
            })
        );
        // Two bytes of size: 0x80 | 1, 1 is 129.
        assert_eq!(read_obu_header(&[0x32, 0x81, 0x01]).map(|header| header.payload_size), Ok(129));
        // No size field; no data; a size that does not end; the forbidden bit; a type that does not
        // exist; the reserved bit; reserved bits of the extension.
        assert_eq!(read_obu_header(&[0x30, 1, 2]), Err(sys::AOM_CODEC_UNSUP_BITSTREAM));
        assert_eq!(read_obu_header(&[]), Err(sys::AOM_CODEC_CORRUPT_FRAME));
        assert_eq!(read_obu_header(&[0x32, 0x80]), Err(sys::AOM_CODEC_CORRUPT_FRAME));
        assert_eq!(read_obu_header(&[0x92, 0x00]), Err(sys::AOM_CODEC_CORRUPT_FRAME));
        assert_eq!(read_obu_header(&[0x4a, 0x00]), Err(sys::AOM_CODEC_CORRUPT_FRAME));
        assert_eq!(read_obu_header(&[0x13, 0x00]), Err(sys::AOM_CODEC_CORRUPT_FRAME));
        assert_eq!(read_obu_header(&[0x16, 0x01, 0x00]), Err(sys::AOM_CODEC_CORRUPT_FRAME));
        assert_eq!(read_obu_header(&[0x16]), Err(sys::AOM_CODEC_CORRUPT_FRAME));
    }

    #[test]
    fn the_frame_of_the_highest_layer_is_output() {
        // A frame OBU of spatial layer 1 (extension 0x08) with one byte of payload.
        let layer1 = [0x36, 0x08, 0x01, 0xaa];
        let delimiter = [0x12, 0x00];
        // Nothing follows; a frame of a higher layer follows; of the same layer; behind a delimiter.
        assert!(output_before(&[], 0, 0));
        assert!(!output_before(&layer1, 0, 0));
        assert!(output_before(&layer1, 1, 0));
        assert!(output_before(&[&delimiter[..], &layer1[..]].concat(), 0, 0));
        // The operating point: layer 1 is in 0x301 (spatial layers 0 and 1, temporal layer 0) and not
        // in 0x101.
        assert!(!output_before(&layer1, 0, 0x301));
        assert!(output_before(&layer1, 0, 0x101));
        // Zero bytes behind the frame, and an OBU that is cut.
        assert!(output_before(&[0, 0, 0], 0, 0));
        assert!(output_before(&[0x36, 0x08, 0x05, 0xaa], 0, 0));
    }

    /// Writes bits, the most significant first.
    #[derive(Default)]
    struct Writer {
        bytes: Vec<u8>,
        count: usize,
    }

    impl Writer {
        fn put(&mut self, value: u32, count: u32) -> &mut Self {
            for bit in (0..count).rev() {
                if self.count % 8 == 0 {
                    self.bytes.push(0);
                }
                let last = self.bytes.len() - 1;
                self.bytes[last] |= (((value >> bit) & 1) as u8) << (7 - self.count % 8);
                self.count += 1;
            }
            self
        }
    }

    /// A sequence header of profile 0 for 3840x2160, 10 bits, BT.2020 with PQ, one operating point.
    fn sequence_header() -> Vec<u8> {
        let mut w = Writer::default();
        w.put(0, 3).put(0, 1).put(0, 1); // profile, still_picture, reduced_still_picture_header
        w.put(0, 1).put(0, 1).put(0, 5); // timing info, initial display delay, operating points - 1
        w.put(0, 12).put(12, 5).put(0, 1); // idc, level 12 (above 7: a tier bit)
        w.put(11, 4).put(11, 4).put(3839, 12).put(2159, 12); // the size
        w.put(0, 1); // frame ids
        w.put(1, 1).put(1, 1).put(1, 1); // 128x128, filter intra, intra edge
        w.put(1, 4); // interintra, masked, warped, dual filter
        w.put(1, 1).put(1, 2); // order hint, jnt comp and ref frame mvs
        w.put(1, 1); // choose screen content tools: 2
        w.put(1, 1); // choose integer mv
        w.put(6, 3); // order hint bits
        w.put(1, 1).put(1, 1).put(1, 1); // superres, cdef, restoration
        w.put(1, 1); // high bit depth
        w.put(0, 1); // monochrome
        w.put(1, 1).put(9, 8).put(16, 8).put(9, 8); // the colour description
        w.put(0, 1); // range
        w.put(2, 2); // chroma sample position
        w.bytes.clone()
    }

    #[test]
    fn the_stream_info_of_a_temporal_unit() {
        let sequence = sequence_header();
        let mut unit = vec![0x12, 0x00, 0x0a, sequence.len() as u8];
        unit.extend_from_slice(&sequence);
        // A frame OBU of a key frame: show_existing_frame 0, frame_type 0.
        unit.extend_from_slice(&[0x32, 0x01, 0x00]);
        let info = peek_stream_info(&unit).expect("the unit can be read");
        assert_eq!((info.width, info.height, info.bit_depth), (3840, 2160, 10));
        assert!(info.is_key_frame && info.color_description_present && !info.monochrome);
        assert_eq!((info.color_primaries, info.transfer_characteristics, info.matrix_coefficients), (9, 16, 9));
        assert_eq!((info.color_range, info.chroma_sample_position), (0, 2));

        // An inter frame (frame_type 1) is no key frame; a unit without a sequence header neither.
        let last = unit.len() - 1;
        unit[last] = 0x20;
        assert_eq!(peek_stream_info(&unit).map(|info| (info.width, info.is_key_frame)), Ok((3840, false)));
        assert_eq!(
            peek_stream_info(&[0x12, 0x00, 0x32, 0x01, 0x00]).map(|info| (info.width, info.is_key_frame)),
            Ok((0, false))
        );
        // A sequence header that ends in its fields, a payload that is cut, no data.
        let mut cut = vec![0x0a, 4];
        cut.extend_from_slice(&sequence[..4]);
        assert_eq!(peek_stream_info(&cut), Err(sys::AOM_CODEC_CORRUPT_FRAME));
        assert_eq!(peek_stream_info(&[0x0a, 0x09, 0x00]), Err(sys::AOM_CODEC_CORRUPT_FRAME));
        assert_eq!(peek_stream_info(&[]), Err(sys::AOM_CODEC_INVALID_PARAM));
        // reduced_still_picture_header without still_picture.
        assert_eq!(peek_stream_info(&[0x0a, 0x02, 0x08, 0x00]), Err(sys::AOM_CODEC_UNSUP_BITSTREAM));
    }
}
