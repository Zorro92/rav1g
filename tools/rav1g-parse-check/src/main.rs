// SPDX-License-Identifier: BSD-2-Clause
//
// This source code is subject to the terms of the BSD 2 Clause License and the Alliance for Open Media
// Patent License 1.0. If the BSD 2 Clause License was not distributed with this source code in the LICENSE
// file, you can obtain it at www.aomedia.org/license/software. If the Alliance for Open Media Patent
// License 1.0 was not distributed with this source code in the PATENTS file, you can obtain it at
// www.aomedia.org/license/patent.

//! Holds the parser of phase 2 against the entropy decoder of libaom (docs/phase2-rav1d.md, step 2).
//!
//! Both parse the same stream, temporal unit by temporal unit, each into plain memory in the layouts
//! the GPU stages read (`plain`): the entropy decoder of libaom with a host that has no GPU (`chost`),
//! rav1d's parse pass with the sink of `rav1g-parse` (`rhost`). Every frame of one is then held against
//! the same frame of the other (`compare`): mode info records, grid, transform block records,
//! coefficients, block lists, palettes, loop restoration units.
//!
//! The block lists of both are counted by the D3D12 host's own function, `av1_mi_push_block`
//! (`crates/rav1g/src/host/blocks.rs`), which is compiled into this program as it is (`host`).
//!
//! ```text
//! rav1g-parse-check [-threads <n>] [-parseopt <list>] [-details <n>] [-root <dir>] [-manifest <task list>] [<stream>...]
//! rav1g-parse-check -time [-threads <n>] [-parseopt <list>] <stream>
//! rav1g-parse-check -digest [-threads <n>] [-root <dir>] [-manifest <task list>] [<stream>...]
//! ```
//!
//! `-parseopt` takes the switches of the parser of phase 2 (`rav1g_parse::options`: `nolfmask` and
//! `tilethread`, which are on without it, `none`, `default`, `latelists`), separated by commas: the
//! buffers have to be the same with each of them. With `-time`, `time` adds the time of the sink's calls.
//!
//! `-digest` prints for every stream the line `rav1g-dec -parsedigest` prints on Windows: what rav1d's
//! parse pass reads from it, folded into one number (`rav1g_parse::digest`).
//!
//! One line per stream (`PARSE <name>: EQUAL ...` or `DIFFERENT ...` with the first differences), a
//! summary line, and exit code 1 unless every stream is equal.

#[cfg(windows)]
compile_error!(
    "rav1g-parse-check is the entropy decoder of libaom with a host in plain memory: it does not build for Windows, \
     where rav1g-sys is built for the D3D12 host. On Windows the parser of phase 2 is in rav1g-dec (-parsedigest)."
);

mod chost;
mod compare;
mod container;
mod host;
mod plain;
mod rhost;
mod timing;

/// The bindings of the entropy decoder under the name the host's files know them by.
mod sys {
    pub use rav1g_sys::*;
}

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use compare::Report;
use rav1g_parse::options::ParseOptions;

struct Options {
    threads: u16,
    /// -parseopt: the switches of rav1d's side.
    parse: ParseOptions,
    details: usize,
    time: bool,
    digest: bool,
    streams: Vec<(String, PathBuf)>,
}

fn usage() -> ! {
    eprintln!(
        "usage: rav1g-parse-check [-threads <n>] [-parseopt <list>] [-details <n>] [-root <dir>] [-manifest <task list>] \
         [<stream>...]\n       rav1g-parse-check -time [-threads <n>] [-parseopt <list>] <stream>\n-parseopt: the \
         switches of the parser, separated by commas: nolfmask, tilethread (both on by default); none; default; \
         latelists; time (with -time)"
    );
    std::process::exit(2);
}

/// The streams of a task list of the conformance run: `-name <name> -i <path>` in every line.
fn manifest(file: &Path, root: &Path) -> Vec<(String, PathBuf)> {
    let text = std::fs::read_to_string(file).unwrap_or_else(|error| {
        eprintln!("rav1g-parse-check: cannot read {}: {error}", file.display());
        std::process::exit(2);
    });
    let mut streams = Vec::new();
    for line in text.lines() {
        let words: Vec<&str> = line.split_whitespace().collect();
        let value = |option: &str| words.iter().position(|word| *word == option).and_then(|at| words.get(at + 1));
        if let Some(path) = value("-i") {
            let name = value("-name").unwrap_or(path);
            streams.push((String::from(*name), root.join(path)));
        }
    }
    streams
}

fn options() -> Options {
    let mut options = Options {
        threads: 1,
        parse: ParseOptions::default(),
        details: 8,
        time: false,
        digest: false,
        streams: Vec::new(),
    };
    let mut root = PathBuf::from(".");
    let mut manifests = Vec::new();
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        let mut value = || args.next().unwrap_or_else(|| usage());
        match arg.as_str() {
            "-threads" => options.threads = value().parse().unwrap_or_else(|_| usage()),
            "-details" => options.details = value().parse().unwrap_or_else(|_| usage()),
            "-root" => root = PathBuf::from(value()),
            "-manifest" => manifests.push(PathBuf::from(value())),
            "-time" => options.time = true,
            "-parseopt" => options.parse = ParseOptions::from_words(&value()).unwrap_or_else(|| usage()),
            "-digest" => options.digest = true,
            _ if arg.starts_with('-') => usage(),
            _ => {
                let path = PathBuf::from(&arg);
                let name = path.file_name().map_or_else(|| arg.clone(), |name| name.to_string_lossy().into_owned());
                options.streams.push((name, path));
            }
        }
    }
    for file in manifests {
        options.streams.extend(manifest(&file, &root));
    }
    if options.streams.is_empty() {
        usage();
    }
    options
}

/// Both parsers on one stream. `Err` when the stream could not be read or a parser not created.
fn check(path: &Path, options: &Options) -> Result<Report, String> {
    let data = std::fs::read(path).map_err(|error| format!("cannot read the file: {error}"))?;
    let units = container::read(&data)?;
    let mut c = chost::CParser::new()?;
    let mut r = rhost::RustParser::new(options.threads, options.parse)?;
    let mut report = Report::new(options.details);
    let mut number = 0;
    for (unit, data) in units.iter().enumerate() {
        let c_result = c.send(data);
        let r_result = r.send(data);
        if c_result.is_err() != r_result.is_err() {
            report.difference("decode error", || {
                format!("temporal unit {unit}: libaom {c_result:?}, rav1d {r_result:?}")
            });
        }
        let c_frames = c.take_frames();
        let r_frames = r.take_frames();
        if c_frames.len() != r_frames.len() {
            report.difference("frame count", || {
                format!("temporal unit {unit}: {} frames with libaom, {} with rav1d", c_frames.len(), r_frames.len())
            });
        }
        for (c_frame, r_frame) in c_frames.iter().zip(&r_frames) {
            compare::frame(&mut report, number, c_frame, r_frame);
            number += 1;
        }
    }
    if report.frames == 0 {
        return Err(String::from("no frame was parsed"));
    }
    Ok(report)
}

/// `-digest`: the line `rav1g-dec -parsedigest` prints for a stream on Windows, here.
fn digest(options: &Options) -> ExitCode {
    let mut code = ExitCode::SUCCESS;
    for (name, path) in &options.streams {
        let result = std::fs::read(path).map_err(|error| format!("cannot read the file: {error}")).and_then(|data| {
            let units = container::read(&data)?;
            let parser = rav1g_parse::digest::DigestParser::new(options.threads)
                .map_err(|error| format!("rav1d was not created: {error:?}"))?;
            for unit in units {
                let _ = parser.send(unit);
            }
            Ok(parser.digest())
        });
        match result {
            Ok(digest) => {
                println!(
                    "PARSEDIGEST {name} frames {} failed {} blocks {} transform_blocks {} coefficients {} hash {:016x}",
                    digest.frames, digest.failed_frames, digest.blocks, digest.tx_blocks, digest.coefs, digest.hash
                );
                if digest.frames == 0 {
                    code = ExitCode::FAILURE;
                }
            }
            Err(error) => {
                println!("ERROR {name}: {error}");
                code = ExitCode::FAILURE;
            }
        }
    }
    code
}

fn main() -> ExitCode {
    let options = options();
    if options.time {
        return timing::run(&options.streams[0].1, options.threads, options.parse);
    }
    if options.digest {
        return digest(&options);
    }
    let mut total = Report::new(0);
    let mut equal = 0;
    for (name, path) in &options.streams {
        match check(path, &options) {
            Ok(report) => {
                let counts = format!(
                    "frames {} records {} transform blocks {} coefficients {} palette planes {} loop restoration units {}",
                    report.frames, report.records, report.tx_blocks, report.coefs, report.palettes, report.lr_units
                );
                if report.equal() {
                    equal += 1;
                    println!("PARSE {name}: EQUAL {counts}");
                } else {
                    let kinds: Vec<String> =
                        report.differences.iter().map(|(kind, count)| format!("{kind} {count}")).collect();
                    println!("PARSE {name}: DIFFERENT {counts}; differences: {}", kinds.join(", "));
                    for detail in &report.details {
                        println!("    {detail}");
                    }
                }
                total.add(&report);
            }
            Err(error) => println!("PARSE {name}: ERROR {error}"),
        }
    }
    let kinds: Vec<String> = total.differences.iter().map(|(kind, count)| format!("{kind} {count}")).collect();
    println!(
        "PARSE SUMMARY: {equal} of {} streams equal; frames {} records {} transform blocks {} coefficients {} palette \
         planes {} loop restoration units {}{}",
        options.streams.len(),
        total.frames,
        total.records,
        total.tx_blocks,
        total.coefs,
        total.palettes,
        total.lr_units,
        if kinds.is_empty() { String::new() } else { format!("; differences: {}", kinds.join(", ")) }
    );
    let kinds: Vec<String> = total.coverage.iter().map(|(kind, count)| format!("{kind} {count}")).collect();
    println!("PARSE COVERAGE (blocks of each kind among the records compared): {}", kinds.join("; "));
    if equal == options.streams.len() { ExitCode::SUCCESS } else { ExitCode::FAILURE }
}
