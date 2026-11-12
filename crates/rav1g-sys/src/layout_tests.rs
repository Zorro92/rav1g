// SPDX-License-Identifier: BSD-2-Clause
//
// This source code is subject to the terms of the BSD 2 Clause License and the Alliance for Open Media
// Patent License 1.0. If the BSD 2 Clause License was not distributed with this source code in the LICENSE
// file, you can obtain it at www.aomedia.org/license/software. If the Alliance for Open Media Patent
// License 1.0 was not distributed with this source code in the PATENTS file, you can obtain it at
// www.aomedia.org/license/patent.

//! Layout tests: the bindings against the C compiler that built the decoder, and the mode info record
//! against the struct the shaders read it with.
//!
//! bindgen computes layouts with libclang and asserts them when the bindings are compiled. That says that
//! Rust agrees with clang. The decoder is built by MSVC (or by clang-cl in the Linux build), so the first
//! test here asks that compiler: `build.rs` writes a C file with the size and alignment of every probed
//! struct and the offset and size of every one of its fields, compiles it into the library, and writes the
//! same table from the bindings; the two must be equal entry by entry.

use std::collections::HashMap;
use std::ffi::CStr;
use std::mem::{offset_of, size_of};

use crate::shared::{self, MB_MODE_INFO};
use crate::*;

fn field_size<T, F>(_: fn(&T) -> &F) -> usize {
    size_of::<F>()
}

include!(concat!(env!("OUT_DIR"), "/layout_table.rs"));

#[repr(C)]
struct LayoutEntry {
    name: *const std::ffi::c_char,
    size: u32,
    offset: u32,
    align: u32,
}

unsafe extern "C" {
    fn rav1g_layout_table(count: *mut u32) -> *const LayoutEntry;
}

/// The table of layout_probe.c: (name, size, offset, alignment).
fn c_layouts() -> Vec<(String, usize, usize, usize)> {
    let mut count = 0u32;
    // SAFETY: returns a static table of `count` entries with static NUL-terminated names.
    unsafe {
        let table = rav1g_layout_table(&mut count);
        std::slice::from_raw_parts(table, count as usize)
            .iter()
            .map(|entry| {
                let name = CStr::from_ptr(entry.name).to_string_lossy().into_owned();
                (name, entry.size as usize, entry.offset as usize, entry.align as usize)
            })
            .collect()
    }
}

#[test]
fn bindings_and_c_compiler_agree() {
    let c = c_layouts();
    let rust = rust_layouts();
    assert_eq!(c.len(), rust.len(), "entries of the C table and of the table of the bindings");
    let mut types = 0;
    for (c, rust) in c.iter().zip(&rust) {
        assert_eq!(
            (c.0.as_str(), c.1, c.2, c.3),
            *rust,
            "(name, size, offset, alignment): the C compiler of the build, then the bindings"
        );
        types += usize::from(!c.0.contains('.'));
    }
    // 10 structs of the C interface and 13 of blockd.h and mv.h, with their fields.
    assert_eq!(types, 23);
    assert!(c.len() - types >= 245, "{} fields", c.len() - types);
    println!("{types} structs, {} fields: sizes, alignments and offsets equal", c.len() - types);
}

/// The record sizes the host and the shaders count in (libav1/dx/shaders: `ModeInfoSize`, the 16-byte
/// prediction and transform block records, the 48-byte warp block record; the same numbers are in the
/// layout lines of the stage dumps, testing/common/stage_dump.h).
#[test]
fn gpu_record_sizes() {
    assert_eq!(size_of::<MB_MODE_INFO>(), 224);
    assert_eq!(size_of::<shared::PredictionBlock>(), 16);
    assert_eq!(size_of::<shared::InterWarpBlock>(), 48);
    assert_eq!(size_of::<shared::tx_block_info_gpu>(), 16);
    assert_eq!(size_of::<shared::PredictionBlockSortingInfo>(), 8);
    assert_eq!(size_of::<shared::MV>(), 4);
    assert_eq!(size_of::<shared::int_mv>(), 4);
}

/// The structs of an HLSL header as a structured buffer lays them out: scalars and vectors of 32-bit
/// words one behind the other, no padding. name -> (size in bytes, fields: (name, offset, size)).
fn hlsl_structs(source: &str) -> HashMap<String, (usize, Vec<(String, usize, usize)>)> {
    // Comments out.
    let mut text = String::new();
    let mut rest = source;
    while let Some(start) = rest.find("/*") {
        text.push_str(&rest[..start]);
        rest = rest[start..].find("*/").map_or("", |end| &rest[start + end + 2..]);
    }
    text.push_str(rest);
    let text: String = text.lines().map(|line| line.split("//").next().unwrap_or("")).collect::<Vec<_>>().join("\n");

    let mut structs: HashMap<String, (usize, Vec<(String, usize, usize)>)> = HashMap::new();
    let mut rest = text.as_str();
    while let Some(start) = rest.find("struct ") {
        rest = &rest[start + "struct ".len()..];
        let open = rest.find('{').expect("a struct has a body");
        let close = rest.find('}').expect("a struct body ends");
        let name = rest[..open].trim().to_string();
        let mut offset = 0;
        let mut fields = Vec::new();
        for declaration in rest[open + 1..close].split(';') {
            let declaration = declaration.trim();
            if declaration.is_empty() {
                continue;
            }
            let (kind, field) = declaration.split_once(char::is_whitespace).expect("<type> <name>");
            let field = field.trim();
            let (field, count) = match field.split_once('[') {
                Some((field, count)) => (field, count.trim_end_matches(']').trim().parse::<usize>().expect("a count")),
                None => (field, 1),
            };
            let element = match kind {
                "int" | "uint" => 4,
                "int2" | "uint2" => 8,
                "int4" | "uint4" => 16,
                other => structs.get(other).unwrap_or_else(|| panic!("{name}: unknown type {other}")).0,
            };
            fields.push((field.trim().to_string(), offset, element * count));
            offset += element * count;
        }
        structs.insert(name, (offset, fields));
        rest = &rest[close..];
    }
    structs
}

/// `libav1/dx/shaders/mode_info.h` is the shaders' view of the mode info pool, which the entropy decoder
/// fills with `MB_MODE_INFO` records of blockd.h: gen_pred_blocks.hlsl and the loop filter shaders read it
/// as `StructuredBuffer<MB_MODE_INFO>`. Every field of the shader's struct must lie where the C field (or
/// the first of the C fields it packs into one word) lies, with the same size.
#[test]
fn mode_info_as_the_shaders_read_it() {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../libav1/dx/shaders/mode_info.h");
    let source = std::fs::read_to_string(path).expect("libav1/dx/shaders/mode_info.h");
    let structs = hlsl_structs(&source);
    let define = source.lines().find_map(|line| line.strip_prefix("#define ModeInfoSize ")).expect("ModeInfoSize");
    assert_eq!(define.trim().parse::<usize>().unwrap(), size_of::<MB_MODE_INFO>());

    let (size, fields) = &structs["MB_MODE_INFO"];
    assert_eq!(*size, size_of::<MB_MODE_INFO>());
    macro_rules! at {
        ($first:ident $(, $more:ident)*) => {{
            // The C fields of one shader word, one byte or one 16-bit value each, in the order of their
            // bytes; the offset of the first and the size of all.
            let offset = offset_of!(MB_MODE_INFO, $first);
            #[allow(unused_mut)]
            let mut end = offset + field_size(|s: &MB_MODE_INFO| &s.$first);
            $(
                assert_eq!(offset_of!(MB_MODE_INFO, $more), end, concat!(stringify!($more), " follows"));
                end += field_size(|s: &MB_MODE_INFO| &s.$more);
            )*
            (offset, end - offset)
        }};
    }
    // The shader's field, and where its bytes are in the C struct.
    let expected: &[(&str, (usize, usize))] = &[
        ("palette_mode_info", at!(palette_mode_info)),
        ("wm_params", at!(wm_params)),
        ("interinter_comp", at!(interinter_comp)),
        // FILTER_INTRA_MODE_INFO is two bytes; the shader reads the word they are in.
        ("filter_intra_mode_info", (offset_of!(MB_MODE_INFO, filter_intra_mode_info), 4)),
        ("mv", at!(mv)),
        ("interp_filters", at!(interp_filters)),
        ("interintra_wedge_index", at!(interintra_wedge_index)),
        ("interintra_wedge_sign", at!(interintra_wedge_sign)),
        ("current_qindex", at!(current_qindex)),
        ("delta_lf_from_base", at!(delta_lf_from_base)),
        ("delta_lf", at!(delta_lf)),
        ("mi_row", at!(mi_row)),
        ("mi_col", at!(mi_col)),
        ("index_base", at!(index_base)),
        ("num_proj_ref", at!(num_proj_ref)),
        ("cfl_alpha_idx", at!(cfl_alpha_idx)),
        ("cfl_alpha_signs", at!(cfl_alpha_signs)),
        ("comp_idx", at!(comp_group_idx, compound_idx)),
        ("block_type", at!(sb_type, partition, ref_frame)),
        ("modes", at!(mode, uv_mode, interintra_mode, motion_mode)),
        ("intra_mode_flags", at!(use_wedge_interintra, use_intrabc, angle_delta)),
        ("tx_info", at!(tx_size, skip, skip_mode, segment_id)),
        ("inter_tx_size", at!(inter_tx_size)),
        ("seg_cdef_flags", at!(seg_id_predicted, cdef_strength, ref_mv_idx, reserved)),
    ];
    assert_eq!(fields.len(), expected.len(), "fields of MB_MODE_INFO in mode_info.h");
    for ((name, offset, size), (expected_name, (c_offset, c_size))) in fields.iter().zip(expected) {
        assert_eq!(name, expected_name);
        assert_eq!((*offset, *size), (*c_offset, *c_size), "{name}: (offset, size) in mode_info.h, then in blockd.h");
    }

    // The structs inside it.
    assert_eq!(structs["PALETTE_MODE_INFO"].0, size_of::<shared::PALETTE_MODE_INFO>());
    assert_eq!(structs["WarpedMotionParams"].0, size_of::<shared::WarpedMotionParams>());
    assert_eq!(structs["INTERINTER_COMPOUND_DATA"].0, size_of::<shared::INTERINTER_COMPOUND_DATA>());
    let field = |name: &str, field: &str| {
        let found = structs[name].1.iter().find(|(candidate, _, _)| candidate == field);
        found.map(|(_, offset, size)| (*offset, *size)).unwrap_or_else(|| panic!("{name}.{field} in mode_info.h"))
    };
    // PALETTE_MODE_INFO: 24 colours of 16 bits, then the two sizes of 16 bits.
    assert_eq!(field("PALETTE_MODE_INFO", "colors"), (offset_of!(shared::PALETTE_MODE_INFO, palette_colors), 48));
    assert_eq!(field("PALETTE_MODE_INFO", "sizes"), (offset_of!(shared::PALETTE_MODE_INFO, palette_size), 4));
    // WarpedMotionParams: the matrix, the four 16-bit angles, and the word with the type.
    assert_eq!(field("WarpedMotionParams", "mat"), (offset_of!(shared::WarpedMotionParams, wmmat), 32));
    assert_eq!(field("WarpedMotionParams", "angles"), (offset_of!(shared::WarpedMotionParams, alpha), 8));
    assert_eq!(field("WarpedMotionParams", "type").0, offset_of!(shared::WarpedMotionParams, wmtype));
    // INTERINTER_COMPOUND_DATA: four words.
    assert_eq!(field("INTERINTER_COMPOUND_DATA", "type"), (offset_of!(shared::INTERINTER_COMPOUND_DATA, type_), 4));
    assert_eq!(
        field("INTERINTER_COMPOUND_DATA", "mask_type"),
        (offset_of!(shared::INTERINTER_COMPOUND_DATA, mask_type), 4)
    );
    assert_eq!(
        field("INTERINTER_COMPOUND_DATA", "wedge_index"),
        (offset_of!(shared::INTERINTER_COMPOUND_DATA, wedge_index), 4)
    );
    assert_eq!(
        field("INTERINTER_COMPOUND_DATA", "wedge_sign"),
        (offset_of!(shared::INTERINTER_COMPOUND_DATA, wedge_sign), 4)
    );
}

/// The bytes of `MB_MODE_INFO` that belong to no field. The entropy decoder writes the record field by
/// field and nothing clears the mode info pool, so these bytes hold what the memory held before, and the
/// shaders read the words they are in: `wm_params.type` (byte 93, between `wmtype` and `invalid`) and
/// `filter_intra_mode_info` (bytes 114 and 115, behind `use_filter_intra`). A read of such a word must mask
/// the padding out (docs/gpu-av1-decoder.md, "Padding of the mode info record"; the comments of
/// `libav1/dx/shaders/mode_info.h` mark these bytes). A change of the struct that moves or adds padding
/// fails here: then check every shader read of the word it is in, and update `mode_info.h` and this list.
#[test]
fn mode_info_padding_is_where_the_shaders_mask_it() {
    let mut covered = vec![false; size_of::<MB_MODE_INFO>()];
    let mut cover = |offset: usize, size: usize| covered[offset..offset + size].fill(true);
    macro_rules! fields {
        ($base:expr, $type:ty: $($field:ident),* $(,)?) => {
            $( cover($base + offset_of!($type, $field), field_size(|s: &$type| &s.$field)); )*
        };
    }
    fields!(0, MB_MODE_INFO:
        mv, interp_filters, interintra_wedge_index, interintra_wedge_sign, current_qindex, delta_lf_from_base,
        delta_lf, mi_row, mi_col, index_base, num_proj_ref, cfl_alpha_idx, cfl_alpha_signs, comp_group_idx,
        compound_idx, sb_type, partition, ref_frame, mode, uv_mode, interintra_mode, motion_mode,
        use_wedge_interintra, use_intrabc, angle_delta, tx_size, skip, skip_mode, segment_id, inter_tx_size,
        seg_id_predicted, cdef_strength, ref_mv_idx, reserved,
    );
    // The structs inside it, by their own fields.
    fields!(offset_of!(MB_MODE_INFO, palette_mode_info), shared::PALETTE_MODE_INFO: palette_colors, palette_size);
    fields!(offset_of!(MB_MODE_INFO, wm_params), shared::WarpedMotionParams:
        wmmat, alpha, beta, gamma, delta, wmtype, invalid);
    fields!(offset_of!(MB_MODE_INFO, interinter_comp), shared::INTERINTER_COMPOUND_DATA:
        type_, mask_type, wedge_index, wedge_sign);
    fields!(offset_of!(MB_MODE_INFO, filter_intra_mode_info), shared::FILTER_INTRA_MODE_INFO:
        filter_intra_mode, use_filter_intra);
    let padding: Vec<usize> =
        covered.iter().enumerate().filter(|(_, written)| !**written).map(|(byte, _)| byte).collect();
    assert_eq!(padding, [93, 114, 115], "bytes of MB_MODE_INFO that no field covers");
}
