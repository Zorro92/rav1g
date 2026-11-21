// SPDX-License-Identifier: BSD-2-Clause
//
// This source code is subject to the terms of the BSD 2 Clause License and the Alliance for Open Media
// Patent License 1.0. If the BSD 2 Clause License was not distributed with this source code in the LICENSE
// file, you can obtain it at www.aomedia.org/license/software. If the Alliance for Open Media Patent
// License 1.0 was not distributed with this source code in the PATENTS file, you can obtain it at
// www.aomedia.org/license/patent.

//! Holds the parser of phase 2 against itself (docs/phase2-rav1d.md, step 2): rav1d's parse pass behind
//! the sink of `rav1g-parse`, in the configuration asked for, against the same parse with one thread,
//! one frame in parse and the default switches.
//!
//! Both parse the same stream, temporal unit by temporal unit, each into plain memory in the layouts
//! the GPU stages read (`plain`, `rhost`). Every frame of the run that is checked is then held against
//! the same frame of the reference (`compare`): mode info records, grid, transform block records,
//! coefficients, block lists, palettes, loop restoration units. What the threads, the frames in parse,
//! the switches of the parser and the place of the wavefront must not change is so held equal value by
//! value, and a difference is reported with its first field. That the reference reads a stream
//! correctly is not checked here: the decoder's output is held against the md5 of every stream of the
//! conformance set, and the parse digest (`-digest`) against the other builds.
//!
//! The block lists of both are counted by the D3D12 host's own function, `av1_mi_push_block`
//! (`crates/rav1g/src/host/blocks.rs`), which is compiled into this program as it is (`host`).
//!
//! ```text
//! rav1g-parse-check [-threads <n>] [-frames <n>] [-parseopt <list>] [-wave <way>] [-details <n>] [-root <dir>] [-manifest <task list>] [<stream>...]
//! rav1g-parse-check -time [-frametimes] [-threads <n>] [-frames <n>] [-parseopt <list>] [-wave <way>] <stream>
//! rav1g-parse-check -digest [-threads <n>] [-root <dir>] [-manifest <task list>] [<stream>...]
//! rav1g-parse-check -lrmix [<stream>...]
//! rav1g-parse-check -tiles [<stream>...]
//! ```
//!
//! `-threads`, `-frames` and `-parseopt` are those of the run that is checked; the reference always has
//! one thread, one frame in parse and the sink-written grid. `-coeflegacy` and `-refrowslegacy` give the
//! run that is checked (or timed) the previous paths of the parse, the decoder's -parsecoeflegacy and
//! -parserefrowslegacy: the linked list of AC coefficient tokens, and the wait of a frame in parse for the
//! rows of all seven references.
//!
//! `-frametimes`, with `-time`: one line per frame of every run with the sink, `FRAMETIME run=<r> no=<n>`
//! and the frame's times in milliseconds (rav1d's `parse_stats`): `admit` its wait for a frame context,
//! `wall` from then to its end, `pre` up to its first task, `init` its `Init` and `InitCdf` tasks, `cdf` the
//! wait between them for the CDFs it loads, `tile` its entropy tile tasks (summed over its tiles), `ref` the
//! time without a tile running after a tile waited for the rows of a reference, `idle` the rest of the time
//! without a tile running. With one frame in parse a frame has `admit` and `wall` only.
//!
//! `-parseopt` takes the switches of the parser of phase 2 (`rav1g_parse::options`: `nolfmask` and
//! `tilethread`, which are on without it, `none`, `default`, `latelists`), separated by commas: the
//! buffers have to be the same with each of them. With `-time`, `time` adds the time of the sink's calls.
//! With `gpugrid` the sink of the run that is checked writes no grid and gives the block lists the
//! records at the edges of the tiles; its grid is then built here from its records, as the pass of the
//! GPU builds it in the decoder, and held against the reference's like every other buffer.
//!
//! `-wave` says where the host's function keeps the wavefront of intra prediction
//! (`crates/rav1g/src/host/blocks.rs`): `edges` (the default, as in the decoder: in the edges of the
//! tiles), `grid` (in the wavefront grids, which the decoder then clears for every frame), or `check`:
//! the grids for the reference and the edges for the run that is checked. With `check` the block lists
//! are equal only if every transform block gets the same iteration either way, and the entries of the
//! grid the edges' side writes (the ones the shader reads) have to be those of the grid.
//!
//! `-tiles` prints the tile layouts of every stream (AV1 5.9.15, tile_info): one `TILES` line per layout,
//! with the frames that have it.
//!
//! `-digest` prints for every stream the line `rav1g-dec -parsedigest` prints on Windows: what rav1d's
//! parse pass reads from it, folded into one number (`rav1g_parse::digest`).
//!
//! One line per stream (`PARSE <name>: EQUAL ...` or `DIFFERENT ...` with the first differences), a
//! summary line, and exit code 1 unless every stream is equal.

#[cfg(windows)]
compile_error!(
    "rav1g-parse-check is a tool for Linux: rav1g-sys builds the decoder's shader table for a Windows target, which \
     this program has no use for. On Windows the parser of phase 2 is in rav1g-dec (-parsedigest)."
);

mod compare;
mod container;
mod host;
mod plain;
mod rhost;
mod timing;

/// The types of the decoder's structs under the name the host's files know them by.
mod sys {
    pub use rav1g_sys::*;
}

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use compare::Report;
use rav1g_parse::options::ParseOptions;

struct Options {
    /// -threads: the threads of the run that is checked.
    threads: u16,
    /// -frames: the frames it may have in parse at once (at most the threads).
    frames: u16,
    /// -parseopt: its switches.
    parse: ParseOptions,
    details: usize,
    time: bool,
    digest: bool,
    lr_mix: bool,
    /// -wave: the wavefront in the edges of the tiles, for the reference and for the run that is checked.
    wave_edges: (bool, bool),
    /// -coeflegacy, -refrowslegacy: the previous paths of the parse, for the run that is checked.
    legacy: rhost::Legacy,
    /// -frametimes (with -time): a line with the times of every frame of every run with the sink.
    frame_times: bool,
    /// -tiles: the tile layouts of the streams.
    tiles: bool,
    streams: Vec<(String, PathBuf)>,
}

fn usage() -> ! {
    eprintln!("usage: rav1g-parse-check -lrmix [<stream>...]  (count coded restoration units per frame and plane)");
    eprintln!("usage: rav1g-parse-check -tiles [<stream>...]  (the tile layouts of the frames, AV1 5.9.15)");
    eprintln!(
        "usage: rav1g-parse-check [-threads <n>] [-frames <n>] [-parseopt <list>] [-wave <way>] [-details <n>] \
         [-root <dir>] [-manifest <task list>] [<stream>...]\n       rav1g-parse-check -time [-frametimes] [-threads <n>] \
         [-frames <n>] [-parseopt <list>] [-wave <way>] <stream>\nThe parse with -threads, -frames and -parseopt is \
         held against the parse with one thread, one frame in parse and the default switches.\n-frames: the frames \
         rav1d may have in parse at once (default 1; at most the threads)\n-parseopt: the switches of the parser, \
         separated by commas: nolfmask, tilethread (both on by default); none; default; latelists; time (with \
         -time)\n-wave: where the wavefront of intra prediction is kept: edges (default), grid, check (the grid for \
         the reference, the edges for the run that is checked)\n-coeflegacy, -refrowslegacy: the previous linked AC tokens, \
         the previous wait for the rows of all references\n-frametimes: with -time, a FRAMETIME line per frame of every run"
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
        frames: 1,
        parse: ParseOptions::default(),
        details: 8,
        time: false,
        digest: false,
        lr_mix: false,
        wave_edges: (true, true),
        legacy: rhost::Legacy::default(),
        frame_times: false,
        tiles: false,
        streams: Vec::new(),
    };
    let mut root = PathBuf::from(".");
    let mut manifests = Vec::new();
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        let mut value = || args.next().unwrap_or_else(|| usage());
        match arg.as_str() {
            "-threads" => options.threads = value().parse().unwrap_or_else(|_| usage()),
            "-frames" => options.frames = value().parse().unwrap_or_else(|_| usage()),
            "-details" => options.details = value().parse().unwrap_or_else(|_| usage()),
            "-root" => root = PathBuf::from(value()),
            "-manifest" => manifests.push(PathBuf::from(value())),
            "-time" => options.time = true,
            "-frametimes" => options.frame_times = true,
            "-tiles" => options.tiles = true,
            "-coeflegacy" => options.legacy.coef_tokens = true,
            "-refrowslegacy" => options.legacy.ref_rows = true,
            "-parseopt" => options.parse = ParseOptions::from_words(&value()).unwrap_or_else(|| usage()),
            "-digest" => options.digest = true,
            "-lrmix" => options.lr_mix = true,
            "-wave" => {
                options.wave_edges = match value().as_str() {
                    "edges" => (true, true),
                    "grid" => (false, false),
                    "check" => (false, true),
                    _ => usage(),
                }
            }
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
    if options.parse.gpu_pred_lists && !options.time {
        eprintln!("rav1g-parse-check: gpublists requires -time here. Use rav1g-dec to check the GPU lists.");
        usage();
    }
    if options.streams.is_empty() {
        usage();
    }
    options
}

/// The reference and the run that is checked on one stream. `Err` when the stream could not be read or a
/// parser not created.
fn check(path: &Path, options: &Options) -> Result<Report, String> {
    let data = std::fs::read(path).map_err(|error| format!("cannot read the file: {error}"))?;
    let units = container::read(&data)?;
    // Keep the sink-written grid to check the edge readers, and the linked AC token list to
    // compare the default packed path's transform records and coefficients to the previous path.
    let reference_options = ParseOptions { gpu_grid: false, ..ParseOptions::default() };
    let legacy = rhost::Legacy { coef_tokens: true, ref_rows: false };
    let mut reference = rhost::RustParser::with_clock(1, 1, reference_options, false, options.wave_edges.0, legacy)?;
    let mut checked = rhost::RustParser::with_clock(
        options.threads,
        options.frames,
        options.parse,
        false,
        options.wave_edges.1,
        options.legacy,
    )?;
    let mut report = Report::new(options.details);
    let mut number = 0;
    // With one frame in parse the two are held against each other temporal unit by temporal unit. With
    // more, the run that is checked is given a run of units before its frames are waited for, so that
    // the frames of several units are in parse side by side: one unit more than frames may be in parse,
    // which keeps the frames that are held for the comparison few (a 4K frame is 200 MB of buffers on
    // either side).
    let run = if checked.frames() > 1 { checked.frames() + 1 } else { 1 };
    for (first, units) in units.chunks(run).enumerate().map(|(at, units)| (at * run, units)) {
        let mut reference_frames = Vec::new();
        let mut errors = Vec::new();
        for data in units {
            errors.push((reference.send(data), checked.send(data)));
            reference_frames.extend(reference.take_frames());
        }
        let checked_frames = checked.take_frames();
        if checked.broken() || reference.broken() {
            return Err(String::from("a thread of rav1d ended with a panic"));
        }
        for (unit, (reference_result, checked_result)) in errors.iter().enumerate() {
            // With more than one frame in parse an error of a frame's parse is the sink's to know, not
            // the call's: only what both report for the unit itself is held against each other.
            if reference_result.is_err() != checked_result.is_err() && run == 1 {
                report.difference("decode error", || {
                    format!(
                        "temporal unit {}: reference {reference_result:?}, checked {checked_result:?}",
                        first + unit
                    )
                });
            }
        }
        if reference_frames.len() != checked_frames.len() {
            report.difference("frame count", || {
                format!(
                    "temporal units {first} to {}: {} frames in the reference, {} in the run that is checked",
                    first + units.len() - 1,
                    reference_frames.len(),
                    checked_frames.len()
                )
            });
        }
        for (reference_frame, checked_frame) in reference_frames.iter().zip(&checked_frames) {
            if options.parse.gpu_grid {
                // The sink of the run that is checked wrote no grid: the grid its records give.
                compare::build_grid(checked_frame);
            }
            compare::frame(&mut report, number, reference_frame, checked_frame);
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
    if options.lr_mix {
        let mut code = ExitCode::SUCCESS;
        for (name, path) in &options.streams {
            println!("LR MIX {name}");
            if let Err(error) = lr_mix(path) {
                eprintln!("LR MIX {name}: {error}");
                code = ExitCode::FAILURE;
            }
        }
        return code;
    }
    if options.tiles {
        let mut code = ExitCode::SUCCESS;
        for (_, path) in &options.streams {
            if timing::tiles(path) != ExitCode::SUCCESS {
                code = ExitCode::FAILURE;
            }
        }
        return code;
    }
    if options.time {
        return timing::run(
            &options.streams[0].1,
            options.threads,
            options.frames,
            options.parse,
            options.wave_edges.1,
            options.legacy,
            options.frame_times,
        );
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

/// Count the coded units of planes with restoration. Copy-only planes and show-existing frames have no units.
fn lr_mix(path: &Path) -> Result<(), String> {
    let data = std::fs::read(path).map_err(|error| format!("cannot read the file: {error}"))?;
    let mut parser = rhost::RustParser::new(1, 1, ParseOptions::default(), true)?;
    let mut number = 0;
    let mut total = [[0; 3]; 3];
    for (tu, data) in container::read(&data)?.into_iter().enumerate() {
        parser.send(data).map_err(|error| format!("temporal unit {tu}: {error}"))?;
        for frame in parser.take_frames() {
            let mut counts = [[0; 3]; 3];
            let mut sets = [[0; 16]; 3];
            for unit in frame.lr_units.into_inner().expect("no thread panics with it") {
                let plane = usize::from(unit.plane);
                let counts = &mut counts[plane];
                match i32::from(unit.r#type) {
                    sys::RESTORE_NONE => counts[0] += 1,
                    sys::RESTORE_WIENER => counts[1] += 1,
                    sys::RESTORE_SGRPROJ => {
                        counts[2] += 1;
                        sets[plane][unit.values[0] as usize] += 1;
                    }
                    kind => return Err(format!("unexpected restoration type {kind}")),
                }
            }
            for (plane, [none, wiener, sgr]) in counts.into_iter().enumerate() {
                print!("LR frame {number} plane={plane} none={none} wiener={wiener} sgr={sgr}");
                for (set, count) in sets[plane].into_iter().enumerate().filter(|&(_, count)| count != 0) {
                    print!(" set{set}={count}");
                }
                println!();
                for (total, count) in total[plane].iter_mut().zip([none, wiener, sgr]) {
                    *total += count;
                }
            }
            number += 1;
        }
    }
    if number == 0 {
        return Err(String::from("no frame was parsed"));
    }
    for (plane, [none, wiener, sgr]) in total.into_iter().enumerate() {
        println!("LR total frames={number} plane={plane} none={none} wiener={wiener} sgr={sgr}");
    }
    Ok(())
}
