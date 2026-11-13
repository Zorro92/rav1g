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
 *   5. rav1g_decoder_drain, rav1g_decoder_destroy, rav1g_device_destroy.
 *
 * Colour: the decoder puts out the samples of the stream and converts nothing. What they mean is in
 * Rav1gStreamInfo::color and, with the HDR metadata, in every Rav1gFrame; the application that shows the
 * frames has to use it (docs/color.md).
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
