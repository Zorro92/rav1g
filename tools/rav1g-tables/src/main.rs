// SPDX-License-Identifier: BSD-2-Clause

//! `rav1g-tables`: the host's copies of tables and inline functions of libaom's headers, as text.
//!
//! The D3D12 host (`crates/rav1g/src/host`) has copies of tables that are `static` in a header of
//! libaom (`tables.rs`) and ports of four inline functions of those headers (`blocks.rs`). No C is part
//! of the decoder's build, so nothing of that build can hold them against the headers.
//! `testing/tables_check` does: a C program (`dump.c`, gcc, the headers of `libav1`) prints every table
//! and the functions over a range of inputs, this program prints the same lines from the copies, and the
//! two texts have to be equal.
//!
//! The copies are the host's own files, compiled into this program by path and unchanged, through the
//! module `tools/rav1g-parse-check` has for its block lists (`src/host`: the two files, and what they
//! use of the host's other modules in the form a program without D3D12 needs).
//!
//! A table is one line, `table <name in C> <type of a number> <count of numbers>: <numbers>`, all
//! dimensions in one row. A function is one line per input, `<name in C>(<arguments>) = <result>`.
//! The lines and their order are those of `testing/tables_check/dump.c`.

#[cfg(windows)]
compile_error!(
    "rav1g-tables is a tool for Linux: rav1g-sys builds the decoder's shader table for a Windows target, which this \
     program has no use for."
);

#[path = "../../rav1g-parse-check/src/host/mod.rs"]
#[allow(dead_code, reason = "the module of rav1g-parse-check: this program uses the tables and four functions of it")]
mod host;

/// The types of the decoder's structs under the name the host's files know them by.
mod sys {
    pub use rav1g_sys::*;
}

use std::ffi::c_int;
use std::fmt::{Display, Write as _};
use std::io::Write as _;

use host::blocks::{extract_interp_filter, is_interintra_pred, max_uv_txsize, scale_chroma_bsize_420};
use host::tables;

/// The name C has for the type of a number.
trait Number: Display {
    const NAME: &'static str;
}

impl Number for u8 {
    const NAME: &'static str = "u8";
}

impl Number for i16 {
    const NAME: &'static str = "i16";
}

impl Number for i32 {
    const NAME: &'static str = "i32";
}

fn table<T: Number>(out: &mut String, name: &str, numbers: &[T]) {
    write!(out, "table {name} {} {}:", T::NAME, numbers.len()).expect("writing to a string");
    for number in numbers {
        write!(out, " {number}").expect("writing to a string");
    }
    out.push('\n');
}

/// `BLOCK_INVALID` of `av1/common/enums.h`: what `ss_size_lookup` has where a block size has no plane
/// block for a subsampling.
const BLOCK_INVALID: u8 = 255;

fn main() -> std::io::Result<()> {
    let mut out = String::new();
    table(&mut out, "block_size_wide", &tables::BLOCK_SIZE_WIDE);
    table(&mut out, "block_size_high", &tables::BLOCK_SIZE_HIGH);
    table(&mut out, "ss_size_lookup", &tables::SS_SIZE_LOOKUP);
    table(&mut out, "ss_size_lookup[][1][1]", &tables::SS_SIZE_420);
    table(&mut out, "tx_size_wide", &tables::TX_SIZE_WIDE);
    table(&mut out, "tx_size_high", &tables::TX_SIZE_HIGH);
    table(&mut out, "mi_size_wide_log2", &tables::MI_SIZE_WIDE_LOG2);
    table(&mut out, "mi_size_high_log2", &tables::MI_SIZE_HIGH_LOG2);
    table(&mut out, "max_txsize_rect_lookup", &tables::MAX_TXSIZE_RECT_LOOKUP);
    table(&mut out, "interintra_to_intra_mode", &tables::INTERINTRA_TO_INTRA_MODE);
    table(&mut out, "sm_weight_arrays", &tables::SM_WEIGHT_ARRAYS);
    table(&mut out, "tx_size_wide_log2", &tables::TX_SIZE_WIDE_LOG2);
    table(&mut out, "tx_size_high_log2", &tables::TX_SIZE_HIGH_LOG2);
    table(&mut out, "max_neighbor_obmc", &tables::MAX_NEIGHBOR_OBMC);
    table(&mut out, "quant_dist_weight", &tables::QUANT_DIST_WEIGHT);
    table(&mut out, "quant_dist_lookup_table", &tables::QUANT_DIST_LOOKUP_TABLE);
    table(&mut out, "av1_sub_pel_filters_4", &tables::SUB_PEL_FILTERS_4);
    table(&mut out, "av1_sub_pel_filters_4smooth", &tables::SUB_PEL_FILTERS_4SMOOTH);
    table(&mut out, "av1_bilinear_filters", &tables::BILINEAR_FILTERS);
    table(&mut out, "av1_sub_pel_filters_8", &tables::SUB_PEL_FILTERS_8);
    table(&mut out, "av1_sub_pel_filters_8smooth", &tables::SUB_PEL_FILTERS_8SMOOTH);
    table(&mut out, "av1_sub_pel_filters_8sharp", &tables::SUB_PEL_FILTERS_8SHARP);

    let block_sizes = sys::BLOCK_SIZES_ALL as u8;
    for bsize in 0..block_sizes {
        writeln!(out, "scale_chroma_bsize({bsize}, 1, 1) = {}", scale_chroma_bsize_420(bsize))
            .expect("writing to a string");
    }
    for bsize in 0..block_sizes {
        for ss_x in 0..2 as c_int {
            for ss_y in 0..2 as c_int {
                let at = usize::from(bsize) * 4 + ss_x as usize * 2 + ss_y as usize;
                if tables::SS_SIZE_LOOKUP[at] == BLOCK_INVALID {
                    writeln!(out, "av1_get_max_uv_txsize({bsize}, {ss_x}, {ss_y}): no such plane block")
                } else {
                    let result = max_uv_txsize(bsize, ss_x, ss_y);
                    writeln!(out, "av1_get_max_uv_txsize({bsize}, {ss_x}, {ss_y}) = {result}")
                }
                .expect("writing to a string");
            }
        }
    }
    for bsize in 0..block_sizes {
        for mode in 0..25u8 {
            for ref0 in -1..8i8 {
                for ref1 in -1..8i8 {
                    let result = is_interintra_pred(bsize, mode, [ref0, ref1]);
                    writeln!(out, "is_interintra_pred({bsize}, {mode}, {ref0}, {ref1}) = {}", u8::from(result))
                        .expect("writing to a string");
                }
            }
        }
    }
    let pairs = (0..16u32).flat_map(|y| (0..16u32).map(move |x| (x << 16) | y));
    for filters in pairs.chain([0xfff0_fff0, 0xffff_ffff, 0x1234_5678, 0x8000_0001]) {
        let [v, _, h, _] = filters.to_le_bytes();
        for x_filter in [true, false] {
            writeln!(
                out,
                "av1_extract_interp_filter(0x{filters:08x}, {}) = {}",
                u8::from(x_filter),
                extract_interp_filter([v, h], x_filter)
            )
            .expect("writing to a string");
        }
    }
    std::io::stdout().lock().write_all(out.as_bytes())
}
