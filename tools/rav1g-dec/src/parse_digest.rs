// SPDX-License-Identifier: BSD-2-Clause
//
// This source code is subject to the terms of the BSD 2 Clause License and the Alliance for Open Media
// Patent License 1.0. If the BSD 2 Clause License was not distributed with this source code in the LICENSE
// file, you can obtain it at www.aomedia.org/license/software. If the Alliance for Open Media Patent
// License 1.0 was not distributed with this source code in the PATENTS file, you can obtain it at
// www.aomedia.org/license/patent.

//! `-parsedigest`: the parser of phase 2 (rav1d's parse pass, docs/phase2-rav1d.md) on the stream of
//! every task, through the functions of rav1g.h. One line per stream:
//!
//! ```text
//! PARSEDIGEST <name> frames <n> failed <n> blocks <n> transform_blocks <n> coefficients <n> hash <16 hex digits>
//! ```
//!
//! The line of a stream is the same on every platform and for any number of threads (`-t`);
//! `rav1g-parse-check -digest` prints it on Linux, where the parser is also held against the entropy
//! decoder of libaom, and CI compares the lines.

use rav1g::capi;

use crate::args::{EXIT_ERROR, EXIT_PASS, Task};
use crate::say;
use crate::shim::Reader;

pub fn run(tasks: &[Task]) -> i32 {
    let mut exit_code = EXIT_PASS;
    for task in tasks {
        let Some(mut reader) = Reader::open(&task.input) else {
            say!("ERROR {}: cannot open the stream, or its container is not IVF, WebM, OBU or MP4", task.name);
            exit_code = EXIT_ERROR;
            continue;
        };
        let parser = capi::rav1g_parse_digest_create(task.threads.max(1) as u32);
        if parser.is_null() {
            say!("ERROR {}: the parser was not created", task.name);
            exit_code = EXIT_ERROR;
            continue;
        }
        while reader.next() {
            let data = reader.data();
            // SAFETY: a parser of rav1g_parse_digest_create; the temporal unit of the reader.
            unsafe { capi::rav1g_parse_digest_send(parser, data.as_ptr(), data.len()) };
        }
        let mut result = capi::Rav1gParseDigestResult::default();
        // SAFETY: the parser, which is not used after its destruction.
        unsafe {
            capi::rav1g_parse_digest_result(parser, &mut result);
            capi::rav1g_parse_digest_destroy(parser);
        }
        say!(
            "PARSEDIGEST {} frames {} failed {} blocks {} transform_blocks {} coefficients {} hash {:016x}",
            task.name,
            result.frames,
            result.failed_frames,
            result.blocks,
            result.tx_blocks,
            result.coefs,
            result.hash
        );
        if result.frames == 0 {
            exit_code = EXIT_ERROR;
        }
    }
    exit_code
}
