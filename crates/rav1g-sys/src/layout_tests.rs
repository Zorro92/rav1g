// SPDX-License-Identifier: BSD-2-Clause
//
// This source code is subject to the terms of the BSD 2 Clause License and the Alliance for Open Media
// Patent License 1.0. If the BSD 2 Clause License was not distributed with this source code in the LICENSE
// file, you can obtain it at www.aomedia.org/license/software. If the Alliance for Open Media Patent
// License 1.0 was not distributed with this source code in the PATENTS file, you can obtain it at
// www.aomedia.org/license/patent.

//! Layout tests: the mode info record against the struct the shaders read it with.
//!
//! The layouts of the records are constant assertions of `records.rs`. What those cannot say is that the
//! HLSL reads the same bytes: these tests parse `libav1/dx/shaders/mode_info.h` and hold its structs
//! against the Rust ones, field by field.

use std::collections::HashMap;
use std::mem::{offset_of, size_of};

use crate::records::{self as shared, MB_MODE_INFO};

/// The size of a field, from a function that returns a reference to it.
fn field_size<T, F>(_: fn(&T) -> &F) -> usize {
    size_of::<F>()
}

#[test]
fn gpu_record_sizes() {
    assert_eq!(size_of::<MB_MODE_INFO>(), 96);
    assert_eq!(size_of::<shared::tx_block_info_gpu>(), 16);
    assert_eq!(size_of::<shared::MV>(), 4);
    assert_eq!(size_of::<shared::int_mv>(), 4);
}

type HlslLayout = (usize, Vec<(String, usize, usize)>);

/// The structs of an HLSL header as a structured buffer lays them out: scalars and vectors of 32-bit
/// words one behind the other, no padding. name -> (size in bytes, fields: (name, offset, size)).
fn hlsl_structs(source: &str) -> HashMap<String, HlslLayout> {
    // Comments out.
    let mut text = String::new();
    let mut rest = source;
    while let Some(start) = rest.find("/*") {
        text.push_str(&rest[..start]);
        rest = rest[start..].find("*/").map_or("", |end| &rest[start + end + 2..]);
    }
    text.push_str(rest);
    let text: String = text.lines().map(|line| line.split("//").next().unwrap_or("")).collect::<Vec<_>>().join("\n");

    let mut structs: HashMap<String, HlslLayout> = HashMap::new();
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

/// Each shader field must cover the corresponding Rust fields with no gaps or padding.
#[test]
fn mode_info_as_the_shaders_read_it() {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../libav1/dx/shaders/mode_info.h");
    let source = std::fs::read_to_string(path).expect("libav1/dx/shaders/mode_info.h");
    let structs = hlsl_structs(&source);
    let define = |name| {
        let prefix = format!("#define {name} ");
        source
            .lines()
            .find_map(|line| line.strip_prefix(&prefix))
            .unwrap_or_else(|| panic!("{name} in mode_info.h"))
            .trim()
            .parse::<usize>()
            .unwrap()
    };
    assert_eq!(define("ModeInfoSize"), size_of::<MB_MODE_INFO>());
    assert_eq!(define("MI_POSITION_WORD"), offset_of!(MB_MODE_INFO, mi_row));
    assert_eq!(define("SB_TYPE_WORD"), offset_of!(MB_MODE_INFO, sb_type));
    assert_eq!(define("TX_INFO_WORD"), offset_of!(MB_MODE_INFO, tx_size));
    assert_eq!(define("MI_USE_FILTER_INTRA"), usize::from(MB_MODE_INFO::USE_FILTER_INTRA));
    assert_eq!(define("MI_WARP_INVALID"), usize::from(MB_MODE_INFO::WARP_INVALID));
    assert_eq!(define("MI_MASK_TYPE"), usize::from(MB_MODE_INFO::MASK_TYPE));
    assert_eq!(define("MI_WEDGE_SIGN"), usize::from(MB_MODE_INFO::WEDGE_SIGN));

    let (size, fields) = &structs["MB_MODE_INFO"];
    assert_eq!(*size, size_of::<MB_MODE_INFO>());
    macro_rules! at {
        ($first:ident $(, $more:ident)*) => {{
            // The fields of a shader word in order, with the offset of the first and the size of all.
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
    let expected: &[(&str, (usize, usize))] = &[
        ("warp_mat0", (offset_of!(MB_MODE_INFO, wmmat), 16)),
        ("warp_mat1", (offset_of!(MB_MODE_INFO, wmmat) + 16, field_size(|s: &MB_MODE_INFO| &s.wmmat) - 16)),
        ("warp_angles", at!(warp_angles)),
        ("mv", at!(mv)),
        ("mi_position", at!(mi_row, mi_col)),
        ("index_base", at!(index_base)),
        ("block_type", at!(sb_type, partition, ref_frame)),
        ("modes", at!(mode, uv_mode, interintra_mode, motion_mode)),
        ("intra_mode_flags", at!(use_wedge_interintra, use_intrabc, angle_delta)),
        ("tx_info", at!(tx_size, skip, cdef_strength, segment_id)),
        ("inter_tx_size", at!(inter_tx_size)),
        ("delta_lf", at!(delta_lf)),
        ("intra_params", at!(delta_lf_from_base, cfl_alpha_idx, cfl_alpha_signs, interintra_wedge_index)),
        ("palette_filter_flags", at!(palette_size, filter_intra_mode, flags)),
        ("compound_filters", at!(compound_type, wedge_index, interp_filters)),
    ];
    assert_eq!(fields.len(), expected.len(), "fields of MB_MODE_INFO in mode_info.h");
    for ((name, offset, size), (expected_name, (rust_offset, rust_size))) in fields.iter().zip(expected) {
        assert_eq!(name, expected_name);
        assert_eq!(
            (*offset, *size),
            (*rust_offset, *rust_size),
            "{name}: (offset, size) in mode_info.h and records.rs"
        );
    }

    // The global warp constants still use the original layout.
    assert_eq!(structs["WarpedMotionParams"].0, size_of::<shared::WarpedMotionParams>());
    let field = |field: &str| {
        let found = structs["WarpedMotionParams"].1.iter().find(|(candidate, _, _)| candidate == field);
        found
            .map(|(_, offset, size)| (*offset, *size))
            .unwrap_or_else(|| panic!("WarpedMotionParams.{field} in mode_info.h"))
    };
    // WarpedMotionParams: the matrix, the four 16-bit angles, and the word with the type.
    assert_eq!(field("mat"), (offset_of!(shared::WarpedMotionParams, wmmat), 32));
    assert_eq!(field("angles"), (offset_of!(shared::WarpedMotionParams, alpha), 8));
    assert_eq!(field("type").0, offset_of!(shared::WarpedMotionParams, wmtype));
}
