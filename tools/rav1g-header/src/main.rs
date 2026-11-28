// SPDX-License-Identifier: BSD-2-Clause
//
// This source code is subject to the terms of the BSD 2 Clause License and the Alliance for Open Media
// Patent License 1.0. If the BSD 2 Clause License was not distributed with this source code in the LICENSE
// file, you can obtain it at www.aomedia.org/license/software. If the Alliance for Open Media Patent
// License 1.0 was not distributed with this source code in the PATENTS file, you can obtain it at
// www.aomedia.org/license/patent.

//! `rav1g.h`, the interface of the decoder for C, from the Rust that implements it.
//!
//! `cargo run -p rav1g-header` writes `crates/rav1g/include/rav1g.h` from `crates/rav1g/src/capi.rs` with
//! cbindgen; `cargo run -p rav1g-header -- --check` fails when the file in the tree is not what it would
//! write (CI runs that). The program builds for the machine it runs on: it needs neither Windows nor the
//! decoder.
//!
//! The interface version: `RAV1G_ABI_VERSION` of `capi.rs`, which `rav1g_abi_version` returns, is
//! raised whenever a declaration of the header changes. `abi.txt` has, for each version, the
//! fingerprint of the header's declarations (the text without comments and preprocessor lines, its
//! white space folded, FNV-1a 64); `--check` fails when the declarations of the header have a
//! fingerprint that is not the one of its version, so a change of the interface cannot go out under the
//! version of another. The program prints the fingerprint, for the line of a new version.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

const HEADER: &str = "\
/* SPDX-License-Identifier: BSD-2-Clause
 *
 * This source code is subject to the terms of the BSD 2 Clause License and the Alliance for Open Media
 * Patent License 1.0. If the BSD 2 Clause License was not distributed with this source code in the LICENSE
 * file, you can obtain it at www.aomedia.org/license/software. If the Alliance for Open Media Patent
 * License 1.0 was not distributed with this source code in the PATENTS file, you can obtain it at
 * www.aomedia.org/license/patent.
 */

/* rav1g: an AV1 decoder whose reconstruction runs in D3D12 compute shaders. The interface for C.
 *
 * Written by cbindgen from crates/rav1g/src/capi.rs (cargo run -p rav1g-header). Do not edit.
 *
 * The order of use:
 *   1. rav1g_device_create on the application's D3D12 device and queue (or without, for a device of the
 *      decoder's own); rav1g_device_wait_pipelines.
 *   2. rav1g_peek_stream_info on the temporal units of the stream until one starts with a key frame: the
 *      frame size, the bit depth and the colour description of the sequence.
 *   3. rav1g_settings_default, the size and bit depth of the sequence, the output callbacks;
 *      rav1g_decoder_create.
 *   4. rav1g_decoder_decode per temporal unit. The decoder takes an image from the output for every frame
 *      that will be shown (get_image) and hands the frame over when the GPU has written it (frame).
 *      RAV1G_STATUS_NEEDS_RECONFIGURE: the stream went on with a sequence the decoder does not fit.
 *      A seek: rav1g_decoder_flush, then the temporal units from the new place (docs/integration.md).
 *   5. rav1g_decoder_drain, rav1g_decoder_destroy, rav1g_device_destroy.
 *
 * Colour: the decoder puts out the samples of the stream and converts nothing. What they mean is in
 * Rav1gStreamInfo::color and, with the HDR metadata, in every Rav1gFrame; the application that shows the
 * frames has to use it (docs/color.md).
 *
 * Output: by default an image is a buffer with the planes of the frame, one texture a plane, or both.
 * With Rav1gSettings::output_kind RAV1G_OUTPUT_SEMI_PLANAR it is one NV12 or P010 texture, which the
 * decoder's output pass writes and an application can share with a D3D11 device; the output fence
 * (rav1g_decoder_output_fence, Rav1gFrame::fence_value) says when a frame is written (docs/output.md).
 *
 * A decoder is used by one thread at a time. The callbacks are called from the decoder's threads.
 */";

const AFTER_INCLUDES: &str = "\n\
/* The records of the timing run and of the stage dumps (rav1g_set_perf_callback, rav1g_set_stage_dump)
 * are defined in dx/av1_perf.h of the decoder's sources. */
#ifndef AV1_PERF_H_
typedef struct av1_perf_frame av1_perf_frame;
typedef struct av1_stage_dump av1_stage_dump;
#endif";

fn generate(source: &Path) -> Result<String, String> {
    let config = cbindgen::Config {
        language: cbindgen::Language::C,
        cpp_compat: true,
        header: Some(HEADER.to_string()),
        include_guard: Some("RAV1G_H".to_string()),
        no_includes: true,
        sys_includes: vec!["stdbool.h".to_string(), "stddef.h".to_string(), "stdint.h".to_string()],
        after_includes: Some(AFTER_INCLUDES.to_string()),
        line_length: 110,
        tab_width: 2,
        documentation: true,
        documentation_style: cbindgen::DocumentationStyle::C,
        usize_is_size_t: true,
        sort_by: cbindgen::SortKey::None,
        enumeration: cbindgen::EnumConfig {
            rename_variants: cbindgen::RenameRule::ScreamingSnakeCase,
            prefix_with_name: true,
            ..Default::default()
        },
        ..Default::default()
    };
    let bindings = cbindgen::Builder::new()
        .with_config(config)
        .with_src(source)
        .generate()
        .map_err(|error| format!("cbindgen on {}: {error}", source.display()))?;
    let mut text = Vec::new();
    bindings.write(&mut text);
    String::from_utf8(text).map_err(|error| format!("the header is not UTF-8: {error}"))
}

/// The declarations of a header: the text without comments and preprocessor lines, white space folded.
fn declarations(text: &str) -> String {
    let mut out = String::new();
    let mut rest = text;
    while !rest.is_empty() {
        if let Some(after) = rest.strip_prefix("/*") {
            rest = after.find("*/").map_or("", |end| &after[end + 2..]);
        } else if let Some(after) = rest.strip_prefix("//") {
            rest = after.find('\n').map_or("", |end| &after[end..]);
        } else {
            let c = rest.chars().next().unwrap_or(' ');
            out.push(c);
            rest = &rest[c.len_utf8()..];
        }
    }
    out.lines()
        .filter(|line| !line.trim_start().starts_with('#'))
        .flat_map(str::split_whitespace)
        .collect::<Vec<_>>()
        .join(" ")
}

/// FNV-1a, 64 bits.
fn fingerprint(text: &str) -> u64 {
    text.bytes().fold(0xcbf2_9ce4_8422_2325, |hash, byte| (hash ^ u64::from(byte)).wrapping_mul(0x0100_0000_01b3))
}

/// `RAV1G_ABI_VERSION` as the header defines it.
fn abi_version(text: &str) -> Option<u32> {
    text.lines().find_map(|line| line.trim().strip_prefix("#define RAV1G_ABI_VERSION ")?.trim().parse().ok())
}

/// The fingerprint `abi.txt` has for a version: lines of "<version> <fingerprint in hex>".
fn recorded(table: &str, version: u32) -> Option<u64> {
    table.lines().find_map(|line| {
        let mut words = line.split_whitespace();
        (words.next()?.parse::<u32>().ok()? == version)
            .then(|| u64::from_str_radix(words.next()?.trim_start_matches("0x"), 16).ok())?
    })
}

fn main() -> ExitCode {
    let check = match std::env::args().nth(1).as_deref() {
        None => false,
        Some("--check") => true,
        Some(other) => {
            eprintln!("rav1g-header: unknown argument {other}; usage: rav1g-header [--check]");
            return ExitCode::from(2);
        }
    };
    let repo = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..").join("..");
    let source = repo.join("crates").join("rav1g").join("src").join("capi.rs");
    let header = repo.join("crates").join("rav1g").join("include").join("rav1g.h");
    let text = match generate(&source) {
        Ok(text) => text,
        Err(error) => {
            eprintln!("rav1g-header: {error}");
            return ExitCode::from(2);
        }
    };
    // The version of the interface and the fingerprint of its declarations.
    let print = fingerprint(&declarations(&text));
    let Some(version) = abi_version(&text) else {
        eprintln!("rav1g-header: the header has no RAV1G_ABI_VERSION (capi.rs)");
        return ExitCode::from(2);
    };
    let table_path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("abi.txt");
    let table = std::fs::read_to_string(&table_path).unwrap_or_default();
    match recorded(&table, version) {
        Some(expected) if expected == print => {
            println!("RAV1G_ABI_VERSION {version}: the declarations have the fingerprint of the version ({print:016x})")
        }
        Some(expected) => {
            eprintln!(
                "rav1g-header: the declarations of rav1g.h changed (fingerprint {print:016x}, version {version} has \
                 {expected:016x}): raise RAV1G_ABI_VERSION in crates/rav1g/src/capi.rs and add the line \
                 \"{} {print:016x}\" to tools/rav1g-header/abi.txt",
                version + 1
            );
            return ExitCode::FAILURE;
        }
        None => {
            eprintln!(
                "rav1g-header: tools/rav1g-header/abi.txt has no line for RAV1G_ABI_VERSION {version}: add \
                 \"{version} {print:016x}\""
            );
            return ExitCode::FAILURE;
        }
    }
    if check {
        let current = std::fs::read_to_string(&header).unwrap_or_default();
        if current.replace("\r\n", "\n") == text {
            println!("crates/rav1g/include/rav1g.h is current ({} lines)", text.lines().count());
            return ExitCode::SUCCESS;
        }
        eprintln!(
            "rav1g-header: crates/rav1g/include/rav1g.h is not what crates/rav1g/src/capi.rs gives: \
             run cargo run -p rav1g-header and commit the result"
        );
        return ExitCode::FAILURE;
    }
    if let Err(error) = std::fs::write(&header, &text) {
        eprintln!("rav1g-header: cannot write {}: {error}", header.display());
        return ExitCode::from(2);
    }
    println!("wrote crates/rav1g/include/rav1g.h ({} lines)", text.lines().count());
    ExitCode::SUCCESS
}
