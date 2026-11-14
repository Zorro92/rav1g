// SPDX-License-Identifier: BSD-2-Clause
//
// This source code is subject to the terms of the BSD 2 Clause License and the Alliance for Open Media
// Patent License 1.0. If the BSD 2 Clause License was not distributed with this source code in the LICENSE
// file, you can obtain it at www.aomedia.org/license/software. If the Alliance for Open Media Patent
// License 1.0 was not distributed with this source code in the PATENTS file, you can obtain it at
// www.aomedia.org/license/patent.

//! Writes `libaom_tables.rs`: the constant tables of libaom that the host of the decoder puts into its
//! buffers or computes with (the warp filter, the wedge masters and codebooks, the inter-intra weights,
//! the self-guided filter parameters, the taps of the filter intra modes, the scan orders, the divisor
//! table of the warp shears), as Rust constants.
//!
//! The numbers are read from the C sources of `libav1` where they are defined, as text: the decoder no
//! longer compiles those files (docs/phase2-rav1d.md, step 6), and a copy typed into Rust would be a
//! second place for 12,000 numbers. `tools/rav1g-parse-check` still compiles the C, so the same
//! initialisers are the ones the reference parser runs with.
//!
//! What is read is an initialiser: the braces behind `name[...] = `, comments and nesting dropped, as a
//! flat list of numbers and names. A preprocessor conditional inside one is taken at its first branch,
//! and the script says which condition that has to be.

use std::env;
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};

fn fail(message: &str) -> ! {
    eprintln!("rav1g build.rs: {message}");
    std::process::exit(1);
}

/// A C file without its comments.
fn read_c(path: &Path) -> String {
    println!("cargo:rerun-if-changed={}", path.display());
    let text =
        fs::read_to_string(path).unwrap_or_else(|error| fail(&format!("cannot read {}: {error}", path.display())));
    let bytes = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut at = 0;
    while at < bytes.len() {
        if bytes[at..].starts_with(b"//") {
            while at < bytes.len() && bytes[at] != b'\n' {
                at += 1;
            }
        } else if bytes[at..].starts_with(b"/*") {
            at += 2;
            while at < bytes.len() && !bytes[at..].starts_with(b"*/") {
                at += 1;
            }
            at += 2;
            out.push(' ');
        } else {
            out.push(bytes[at] as char);
            at += 1;
        }
    }
    out
}

#[derive(Clone, Debug, PartialEq)]
enum Token {
    Number(i64),
    Name(String),
}

/// The initialiser of the array `name` in `text`: what stands between the braces behind
/// `name[...]... = {`, as numbers and names. `first_branch`: the condition of the `#if` whose first
/// branch is taken, when the initialiser has one.
fn initialiser(text: &str, file: &str, name: &str, first_branch: Option<&str>) -> Vec<Token> {
    let bytes = text.as_bytes();
    let is_word = |byte: u8| byte.is_ascii_alphanumeric() || byte == b'_';
    let mut from = 0;
    let open = loop {
        let Some(found) = text[from..].find(name) else {
            fail(&format!("{file}: no initialiser of {name}"));
        };
        let start = from + found;
        let mut at = start + name.len();
        from = at;
        if start > 0 && is_word(bytes[start - 1]) || at >= bytes.len() || bytes[at] != b'[' {
            continue;
        }
        // The dimensions, a parenthesis of DECLARE_ALIGNED, then "= {".
        while at < bytes.len() && bytes[at] == b'[' {
            while at < bytes.len() && bytes[at] != b']' {
                at += 1;
            }
            at += 1;
        }
        let rest = text[at..].trim_start();
        let rest = rest.strip_prefix(')').unwrap_or(rest).trim_start();
        if let Some(rest) = rest.strip_prefix('=') {
            let rest = rest.trim_start();
            if rest.starts_with('{') {
                break text.len() - rest.len();
            }
        }
    };
    let mut depth = 0;
    let mut end = open;
    for (index, &byte) in bytes[open..].iter().enumerate() {
        match byte {
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    end = open + index;
                    break;
                }
            }
            _ => {}
        }
    }
    if end == open {
        fail(&format!("{file}: the initialiser of {name} does not end"));
    }
    // Preprocessor lines: the first branch of a conditional.
    let mut body = String::new();
    let mut taking = true;
    let mut conditionals = 0;
    for line in text[open + 1..end].lines() {
        let trimmed = line.trim_start();
        if let Some(directive) = trimmed.strip_prefix('#') {
            let directive = directive.trim();
            if let Some(condition) = directive.strip_prefix("if ") {
                conditionals += 1;
                if conditionals != 1 || Some(condition.trim()) != first_branch {
                    fail(&format!(
                        "{file}: {name} has the conditional \"{directive}\", which the script does not expect"
                    ));
                }
            } else if directive.starts_with("elif") || directive.starts_with("else") {
                taking = false;
            } else if directive.starts_with("endif") {
                taking = true;
            } else {
                fail(&format!("{file}: {name} has the directive \"{directive}\""));
            }
            continue;
        }
        if taking {
            body.push_str(line);
            body.push('\n');
        }
    }
    if first_branch.is_some() && conditionals != 1 {
        fail(&format!("{file}: {name} has no conditional; \"{}\" was expected", first_branch.unwrap_or("")));
    }
    let mut tokens = Vec::new();
    let body = body.as_bytes();
    let mut at = 0;
    let mut negative = false;
    while at < body.len() {
        let byte = body[at];
        if byte == b'-' {
            negative = true;
            at += 1;
        } else if byte.is_ascii_digit() {
            let start = at;
            while at < body.len() && body[at].is_ascii_alphanumeric() {
                at += 1;
            }
            let digits = std::str::from_utf8(&body[start..at]).unwrap_or("");
            let value: i64 =
                digits.parse().unwrap_or_else(|_| fail(&format!("{file}: {name}: \"{digits}\" is not a number")));
            tokens.push(Token::Number(if negative { -value } else { value }));
            negative = false;
        } else if is_word(byte) {
            let start = at;
            while at < body.len() && is_word(body[at]) {
                at += 1;
            }
            if negative {
                fail(&format!("{file}: {name}: a minus in front of a name"));
            }
            tokens.push(Token::Name(String::from_utf8_lossy(&body[start..at]).into_owned()));
        } else if matches!(byte, b'{' | b'}' | b'[' | b']' | b',' | b' ' | b'\t' | b'\n' | b'\r') {
            at += 1;
        } else {
            fail(&format!("{file}: {name}: the character '{}' in its initialiser", byte as char));
        }
    }
    tokens
}

fn numbers(tokens: &[Token], file: &str, name: &str, count: usize) -> Vec<i64> {
    let values: Vec<i64> = tokens
        .iter()
        .map(|token| match token {
            Token::Number(value) => *value,
            Token::Name(word) => fail(&format!("{file}: {name}: the name {word} among its numbers")),
        })
        .collect();
    if values.len() != count {
        fail(&format!("{file}: {name} has {} numbers, {count} were expected", values.len()));
    }
    values
}

/// `pub(crate) static NAME: [[T; inner]; outer] = ...;`: `values` in rows of the dimensions `inner`
/// (none: one dimension).
fn write_table(out: &mut String, rust_name: &str, kind: &str, values: &[i64], inner: &[usize]) {
    fn nest(out: &mut String, values: &[i64], shape: &[usize]) {
        if shape.len() <= 1 {
            for value in values {
                let _ = write!(out, "{value},");
            }
            return;
        }
        for chunk in values.chunks(values.len() / shape[0]) {
            out.push('[');
            nest(out, chunk, &shape[1..]);
            out.push_str("],");
        }
    }
    let outer = values.len() / inner.iter().product::<usize>().max(1);
    let mut dimensions = String::from(kind);
    for &size in inner.iter().rev() {
        dimensions = format!("[{dimensions}; {size}]");
    }
    let mut shape = vec![outer];
    shape.extend(inner.iter().copied());
    let _ = write!(out, "pub(crate) static {rust_name}: [{dimensions}; {outer}] = [");
    nest(out, values, &shape);
    out.push_str("];\n");
}

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    let manifest = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    let common = manifest.parent().and_then(Path::parent).expect("the workspace root").join("libav1/av1/common");
    let mut out = String::from("// Written by build.rs from the C sources of libav1/av1/common.\n");

    // warped_motion.c: the filter of the warp, 64 phases in three ranges and one, and the divisors of
    // the shears.
    let file = "warped_motion.c";
    let text = read_c(&common.join(file));
    let tokens = initialiser(&text, file, "warped_filter", Some("WARPEDPIXEL_PREC_BITS == 6"));
    write_table(&mut out, "WARPED_FILTER", "i16", &numbers(&tokens, file, "warped_filter", 193 * 8), &[8]);
    let tokens = initialiser(&text, file, "div_lut", None);
    write_table(&mut out, "DIV_LUT", "u16", &numbers(&tokens, file, "div_lut", 257), &[]);

    // reconinter.c: the inter-intra weights, and what the wedge masks are built from.
    let file = "reconinter.c";
    let text = read_c(&common.join(file));
    for (name, rust_name, count, inner) in [
        ("ii_weights1d", "II_WEIGHTS1D", 128, &[][..]),
        ("ii_size_scales", "II_SIZE_SCALES", 22, &[][..]),
        ("wedge_master_oblique_odd", "WEDGE_MASTER_OBLIQUE_ODD", 64, &[][..]),
        ("wedge_master_oblique_even", "WEDGE_MASTER_OBLIQUE_EVEN", 64, &[][..]),
        ("wedge_master_vertical", "WEDGE_MASTER_VERTICAL", 64, &[][..]),
        ("wedge_signflip_lookup", "WEDGE_SIGNFLIP", 22 * 16, &[16][..]),
    ] {
        let tokens = initialiser(&text, file, name, None);
        write_table(&mut out, rust_name, "u8", &numbers(&tokens, file, name, count), inner);
    }
    // The codebooks: a direction (WedgeDirectionType) and two offsets in eighths of the block.
    const DIRECTIONS: [&str; 6] = [
        "WEDGE_HORIZONTAL",
        "WEDGE_VERTICAL",
        "WEDGE_OBLIQUE27",
        "WEDGE_OBLIQUE63",
        "WEDGE_OBLIQUE117",
        "WEDGE_OBLIQUE153",
    ];
    const CODEBOOKS: [&str; 3] = ["wedge_codebook_16_hgtw", "wedge_codebook_16_hltw", "wedge_codebook_16_heqw"];
    let mut codebooks = Vec::new();
    for name in CODEBOOKS {
        for token in initialiser(&text, file, name, None) {
            codebooks.push(match token {
                Token::Number(value) => value,
                Token::Name(word) => DIRECTIONS
                    .iter()
                    .position(|&direction| direction == word)
                    .unwrap_or_else(|| fail(&format!("{file}: {name}: {word} is not a wedge direction")))
                    as i64,
            });
        }
    }
    if codebooks.len() != 3 * 16 * 3 {
        fail(&format!("{file}: the wedge codebooks have {} numbers", codebooks.len()));
    }
    write_table(&mut out, "WEDGE_CODEBOOKS", "u8", &codebooks, &[16, 3]);
    // wedge_params_lookup: for every block size its wedge bits and its codebook, as an index into
    // WEDGE_CODEBOOKS; 3 for a block size without wedges.
    let tokens = initialiser(&text, file, "wedge_params_lookup", None);
    let mut params = Vec::new();
    let mut tokens = tokens.iter();
    while let Some(token) = tokens.next() {
        let Token::Number(bits) = token else {
            fail(&format!("{file}: wedge_params_lookup: a row does not start with its bits"));
        };
        let codebook = match tokens.next() {
            Some(Token::Name(word)) if word == "NULL" => 3,
            Some(Token::Name(word)) => CODEBOOKS
                .iter()
                .position(|&codebook| codebook == word)
                .unwrap_or_else(|| fail(&format!("{file}: wedge_params_lookup: the codebook {word}")))
                as i64,
            _ => fail(&format!("{file}: wedge_params_lookup: a row without a codebook")),
        };
        // The sign flips and the masks of the row: wedge_signflip_lookup[BLOCK_..] and
        // wedge_masks[BLOCK_..], or NULL twice.
        let skip = if codebook == 3 { 2 } else { 4 };
        for _ in 0..skip {
            if !matches!(tokens.next(), Some(Token::Name(_))) {
                fail(&format!("{file}: wedge_params_lookup: a row is not as the script expects it"));
            }
        }
        params.extend([*bits, codebook]);
    }
    if params.len() != 22 * 2 {
        fail(&format!("{file}: wedge_params_lookup has {} rows", params.len() / 2));
    }
    write_table(&mut out, "WEDGE_PARAMS", "u8", &params, &[2]);

    // restoration.c: the parameter sets of the self-guided filter, two radii and two strengths each.
    let file = "restoration.c";
    let text = read_c(&common.join(file));
    let tokens = initialiser(&text, file, "sgr_params", None);
    write_table(&mut out, "SGR_PARAMS", "i32", &numbers(&tokens, file, "sgr_params", 16 * 4), &[4]);

    // reconintra.c: the taps of the five filter intra modes.
    let file = "reconintra.c";
    let text = read_c(&common.join(file));
    let tokens = initialiser(&text, file, "av1_filter_intra_taps", None);
    write_table(
        &mut out,
        "FILTER_INTRA_TAPS",
        "i8",
        &numbers(&tokens, file, "av1_filter_intra_taps", 5 * 8 * 8),
        &[8, 8],
    );

    // reconintra.c: for every block size, and for the block sizes of a vertical partition in three,
    // which blocks of a superblock have their top right and their bottom left decoded before them
    // (has_tr_tables, has_bl_tables and the two for PARTITION_VERT_A and _B), a bit a block.
    let mut edge_tables: Vec<String> = Vec::new();
    for (list, rust_name, count) in [
        ("has_tr_tables", "HAS_TR_TABLES", 22),
        ("has_tr_vert_tables", "HAS_TR_VERT_TABLES", 16),
        ("has_bl_tables", "HAS_BL_TABLES", 22),
        ("has_bl_vert_tables", "HAS_BL_VERT_TABLES", 16),
    ] {
        let rows = initialiser(&text, file, list, None);
        if rows.len() != count {
            fail(&format!("{file}: {list} has {} entries, {count} were expected", rows.len()));
        }
        let mut table = format!("pub(crate) static {rust_name}: [&[u8]; {count}] = [");
        for token in &rows {
            let Token::Name(name) = token else {
                fail(&format!("{file}: {list}: a number among its tables"));
            };
            if name == "NULL" {
                table.push_str("&[],");
                continue;
            }
            if !edge_tables.contains(name) {
                let tokens = initialiser(&text, file, name, None);
                let values = numbers(&tokens, file, name, tokens.len());
                let _ = write!(out, "static {}: [u8; {}] = [", name.to_uppercase(), values.len());
                for value in &values {
                    let _ = write!(out, "{value},");
                }
                out.push_str("];\n");
                edge_tables.push(name.clone());
            }
            let _ = write!(table, "&{},", name.to_uppercase());
        }
        table.push_str("];\n");
        out.push_str(&table);
    }

    // scan.c: the three scan orders of every transform size (av1_idct_scans), each scan once.
    let file = "scan.c";
    let text = read_c(&common.join(file));
    let rows = initialiser(&text, file, "av1_idct_scans", None);
    if rows.len() != 19 * 3 {
        fail(&format!("{file}: av1_idct_scans has {} entries", rows.len()));
    }
    let mut scans: Vec<String> = Vec::new();
    let mut table = String::from("pub(crate) static IDCT_SCANS: [[&[i16]; 3]; 19] = [");
    for (index, token) in rows.iter().enumerate() {
        let Token::Name(scan) = token else {
            fail(&format!("{file}: av1_idct_scans: a number among its scans"));
        };
        if !scans.contains(scan) {
            let tokens = initialiser(&text, file, scan, None);
            let values: Vec<i64> = numbers(&tokens, file, scan, tokens.len());
            let _ = write!(out, "static {}: [i16; {}] = [", scan.to_uppercase(), values.len());
            for value in &values {
                let _ = write!(out, "{value},");
            }
            out.push_str("];\n");
            scans.push(scan.clone());
        }
        if index % 3 == 0 {
            table.push('[');
        }
        let _ = write!(table, "&{},", scan.to_uppercase());
        if index % 3 == 2 {
            table.push_str("],");
        }
    }
    table.push_str("];\n");
    out.push_str(&table);

    let path = PathBuf::from(env::var_os("OUT_DIR").expect("OUT_DIR")).join("libaom_tables.rs");
    fs::write(&path, out).unwrap_or_else(|error| fail(&format!("cannot write {}: {error}", path.display())));
}
