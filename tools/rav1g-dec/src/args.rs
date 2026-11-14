// SPDX-License-Identifier: BSD-2-Clause
//
// This source code is subject to the terms of the BSD 2 Clause License and the Alliance for Open Media
// Patent License 1.0. If the BSD 2 Clause License was not distributed with this source code in the LICENSE
// file, you can obtain it at www.aomedia.org/license/software. If the Alliance for Open Media Patent
// License 1.0 was not distributed with this source code in the PATENTS file, you can obtain it at
// www.aomedia.org/license/patent.

//! The arguments: warp_decoder's flags (docs/warp-decoder.md), a task per stream, a list file of tasks.

use crate::say;
use crate::shim;
use rav1g::switches::{self, Clears, Poison};

// The player's values (testing/player_common/cmd_parser.h, decoder_creator.h).
pub const SLOTS_DEFAULT: i32 = 2;
pub const SLOTS_MIN: i32 = 2;
pub const SLOTS_MAX: i32 = 4;
/// -parseframes: the most frames a decoder parses at the same time.
pub const PARSE_FRAMES_MAX: i32 = 4;
pub const LOWMEM_ALL: i32 = 7;
pub const LOWMEM_DEFAULT: i32 = 1;
pub const DECODER_WIDTH: u32 = 4096;
pub const DECODER_HEIGHT: u32 = 2176;
pub const DECODER_BITDEPTH: u32 = 10;
pub const PSO_THREADS: i32 = 4;
pub const QUEUE_SIZE_DEFAULT: i32 = 4;
pub const QUEUE_SIZE_MAX: i32 = 16;

// Exit codes of the process.
/// Every task decoded, and every frame of every task with a reference matched.
pub const EXIT_PASS: i32 = 0;
/// At least one task with a frame that differs from its reference, or a frame count.
pub const EXIT_MISMATCH: i32 = 1;
/// A task could not be decoded (the run stops at that task).
pub const EXIT_ERROR: i32 = 2;
/// Bad arguments, no D3D12 device, no pipeline states.
pub const EXIT_USAGE: i32 = 3;
/// A task ran longer than -timeout (the run stops at that task).
pub const EXIT_TIMEOUT: i32 = 4;

#[derive(Clone)]
pub struct Task {
    pub input: String,
    /// -name, or the file name of the input: what the result line calls the stream.
    pub name: String,
    /// -check: reference md5 file.
    pub check: String,
    /// -md5_frame_check: the reference is <input>.md5, as in the player.
    pub md5_frame_check: bool,
    pub save_md5: String,
    /// -o
    pub out_yuv: String,
    /// -save_wrong_yuv <dir>
    pub save_wrong_yuv: String,
    /// -wrong_limit: at most this many frames per task for -save_wrong_yuv.
    pub wrong_limit: i32,
    /// -aom_md5: monochrome frames are hashed as aomdec --i420 writes them.
    pub aom_md5: bool,
    /// -t: the threads that parse; 0 is the library's default for the frame size.
    pub threads: i32,
    /// -parseframes: the frames whose parse may run at the same time; 0 is the library's default for the
    /// frame size. This program asks for 1 unless it is told otherwise.
    pub parse_frames: i32,
    pub limit: i32,
    pub queue_size: i32,
    /// -slots: the pipeline slots; 0 is the library's default (2 with one frame in parse, with more two
    /// more than frames in parse). 2 unless the task says otherwise, or says -parseframes without -slots:
    /// then 0.
    pub slots: i32,
    /// The task has -slots.
    pub slots_given: bool,
    pub cdefv1: bool,
    pub cdefv2: bool,
    pub nofitsize: bool,
    pub fitsize: bool,
    pub lowmem: bool,
    pub lowmem_parts: i32,
    /// -budget: what the decoder is told the application has left, in MiB; negative: no budget callback.
    pub budget_mib: i64,
    pub intrav2: bool,
    pub intraopt: bool,
    /// What a frame does about the clears of its slot's grids: nothing to clear without a flag;
    /// -gpuclear, -cpuclear (and -asyncclear, its older name), -syncclear.
    pub clears: Clears,
    /// -lfhsplit <0|1|2>
    pub lfhsplit: i32,
    pub intraedgefix: bool,
    /// -poisonmi, -poisonbuf: `switches::set_poison_buffers`.
    pub poison: Poison,
    /// -interfast (1), -interfastlists (7), -interfastmode <n>: `switches::set_inter_fast`.
    pub interfast: u32,
    /// -continue_on_error: a decode call that fails does not end the task; the next temporal units go to
    /// the same decoder.
    pub continue_on_error: bool,
    /// -alllayers: every shown frame of a temporal unit with spatial layers.
    pub all_layers: bool,
    /// -oppoint <n>: the operating point of a stream with layers.
    pub oppoint: i32,
    /// -parser c|rav1d: what reads the bitstream; true is rav1d's parse pass, the default.
    pub parser_rav1d: bool,
    /// -parseopt <list>: the switches of rav1d's parse pass (`rav1g::ParserOptions`).
    pub parser_options: rav1g::ParserOptions,
    /// -timeout: seconds for the task, 0 = none.
    pub timeout_s: i32,
    /// -gputime: the timing run of the player, its summary on stdout.
    pub gputime: bool,
    /// -gputimedetail (1), -gputimeisolate (2): the inter breakdown; either sets gputime.
    pub gputime_detail: i32,
    /// -progress: with -gputime, the per-frame lines behind the summary.
    pub progress: bool,
    /// -dumpstages <frame list>
    pub dumpstages: String,
    /// -dumpdir <dir>
    pub dumpdir: String,
    /// The frames of -dumpstages.
    pub dump_frames: Vec<i32>,
}

impl Default for Task {
    fn default() -> Task {
        Task {
            input: String::new(),
            name: String::new(),
            check: String::new(),
            md5_frame_check: false,
            save_md5: String::new(),
            out_yuv: String::new(),
            save_wrong_yuv: String::new(),
            wrong_limit: -1,
            aom_md5: false,
            threads: 1,
            limit: -1,
            queue_size: QUEUE_SIZE_DEFAULT,
            slots: SLOTS_DEFAULT,
            slots_given: false,
            parse_frames: 1,
            cdefv1: false,
            cdefv2: false,
            nofitsize: false,
            fitsize: false,
            lowmem: false,
            lowmem_parts: LOWMEM_DEFAULT,
            budget_mib: -1,
            intrav2: false,
            intraopt: false,
            clears: Clears::NotNeeded,
            lfhsplit: 0,
            intraedgefix: false,
            poison: Poison::Off,
            interfast: 0,
            continue_on_error: false,
            all_layers: false,
            parser_rav1d: true,
            parser_options: rav1g::ParserOptions::default(),
            oppoint: 0,
            timeout_s: 0,
            gputime: false,
            gputime_detail: 0,
            progress: false,
            dumpstages: String::new(),
            dumpdir: "dumps".to_string(),
            dump_frames: Vec::new(),
        }
    }
}

#[derive(Default)]
pub struct Globals {
    /// -hw
    pub hardware: bool,
    /// -v
    pub verbose: bool,
    /// -debuglayer
    pub debug_layer: bool,
    /// -gpuvalidation
    pub gpu_validation: bool,
    /// -sm6test
    pub sm6test: bool,
    /// -capi
    pub capi: bool,
    /// -keep_going
    pub keep_going: bool,
    /// -memstate
    pub memstate: bool,
    /// -parsedigest
    pub parse_digest: bool,
    /// -list
    pub list: String,
    /// -playersmoke
    pub player_smoke: String,
}

pub fn usage() {
    say!("rav1g-dec: decodes AV1 streams with rav1g on D3D12 (WARP by default), one md5 per output frame.");
    say!("");
    say!("  rav1g-dec -i <stream> [task flags] [-hw] [-v]");
    say!("  rav1g-dec -list <file> [task flags as defaults for every line] [-hw] [-v]");
    say!("  rav1g-dec -sm6test [-hw]");
    say!("  rav1g-dec -playersmoke <directory> [-hw]");
    say!("");
    say!("Task flags:");
    say!("  -i <file>             the stream: .ivf, .mkv/.webm, .obu, .mp4");
    say!("  -name <text>          what the BEGIN and RESULT lines call the stream (default: its file name)");
    say!("  -check <file>         compare with this md5 file: the first 32 characters of each line are the md5 of");
    say!("                        one output frame (the player's format and that of AOM's test vectors)");
    say!("  -md5_frame_check      the same with <stream>.md5, as in the player");
    say!("  -save_md5 <file>      write the md5 of every output frame in the player's format");
    say!("  -aom_md5              hash a monochrome frame with two grey chroma planes, as aomdec --i420 does");
    say!("                        (AOM's .md5 files); without it luma only, as the player and dav1d do");
    say!("  -o <file>             write the visible samples of every output frame (planar, as they are hashed)");
    say!("  -save_wrong_yuv <dir> with a reference: write each frame that differs to <dir>/<name>_<frame>.yuv");
    say!("  -wrong_limit <n>      -save_wrong_yuv writes at most n frames per task");
    say!("  -dumpstages <frames>  write the buffers behind every GPU stage of these frames (decoding order from");
    say!("                        0, hidden frames count; a list like 0,3,10-12) and the lists the stages read");
    say!("                        (mode info, block records, loop filter edge lists) to <dir>/<name>_f<n>.stages,");
    say!("                        as the player does; compare two runs with testing/stage_dump/stage_dump.py");
    say!("  -dumpdir <dir>        where -dumpstages writes (default dumps; created when it is not there)");
    say!("  -limit <n>            decode only the first n temporal units of the stream");
    say!("  -t <n>                decoder threads: the tiles of a frame and, with -parseframes, the frames");
    say!("                        (default 1; 0: the library's default for the frame size, 2 above 2560x1440)");
    say!("  -parseframes <n>      frames whose parse may run at the same time, 1..4 and at most the threads");
    say!("                        (default 1; 0: the library's default for the frame size). With more than 1");
    say!("                        the frames are parsed side by side and submitted in decode order by a");
    say!("                        thread of the decoder (docs/phase2-rav1d.md, \"Frames in parallel\"); the");
    say!("                        output is the same. Without -slots the decoder then has two slots more than");
    say!("                        frames in parse (at most 4), or what its budget guard leaves it");
    say!("  -queue_size <n>       output images in the pool, 1..{} (default {})", QUEUE_SIZE_MAX, QUEUE_SIZE_DEFAULT);
    say!("  -timeout <seconds>    give up on a task after this time (exit code {})", EXIT_TIMEOUT);
    say!("  -cdefv1 -nofitsize -lowmemparts <n> -lowmem -slots <n> -intrav2 -intraopt");
    say!("                        as in the test player (docs/test-player.md); -cdefv2 and -fitsize (the defaults)");
    say!("                        are accepted");
    say!("  -gpuclear -cpuclear -syncclear");
    say!("                        the wavefront of intra prediction in the grid of the slot, which is then cleared");
    say!("                        for every frame: by the GPU at the end of the frame's command list, by the");
    say!("                        completion thread (-asyncclear is the same), or by the decode thread. Without");
    say!("                        one of them the wavefront is kept at the edges of the tiles and nothing is");
    say!("                        cleared (docs/test-player.md, \"Pipeline slots and clears\")");
    say!("  -budget <MiB>         the decoder is told the application has this much memory left for it: the budget");
    say!("                        guard of the creation (MEMORY budget and MEMORY REFUSED lines, with -v)");
    say!("  -lfhsplit <n>         horizontal loop filter: 1 even rows of blocks, a UAV barrier, odd rows; 2 every");
    say!("                        row of blocks on its own (default 0: one dispatch per plane; same output)");
    say!("  -intraedgefix         8-bit intra: the above edge filter stops where the 10-bit one does (same output)");
    say!("  -poisonmi             debugging: the mode info pool starts filled with the byte 0xA5 instead of with what");
    say!("                        the memory held, so a shader that reads bytes the decoder never wrote shows in");
    say!("                        the output of any stream (same output)");
    say!("  -poisonbuf            the same for every buffer the CPU writes for the shaders, and the constants of");
    say!("                        every stage filled again in every frame before the stage writes them (same output)");
    say!("  -parser c|rav1d       what reads the bitstream: the parse pass of rav1d (rav1d, the default) or the");
    say!("                        entropy decoder of libaom (c) (same output; docs/phase2-rav1d.md)");
    say!("  -parseopt <list>      the switches of rav1d's parse, separated by commas (same output). nolfmask: rav1d");
    say!("                        leaves out the masks of its own loop filter; tilethread: a tile stays on its");
    say!("                        thread between its superblock rows. Both are on by default, and a list that names");
    say!("                        one of them, none or default says which are on: none turns both off, nolfmask or");
    say!("                        tilethread leaves only that one on. time: with -gputime, the calls of the sink are");
    say!("                        timed; latelists: the block lists of a tile are counted at its end and not");
    say!("                        superblock by superblock; cleargrid: the mode info grid of a slot is cleared for");
    say!("                        every frame. These three change nothing else (time alone: the default, timed)");
    say!("  -interfast            inter prediction by the fraction of the motion vector: a copy, one filter pass, or");
    say!("                        both (inter_fast, inter_compound_fast; same output)");
    say!("  -interfastlists       the block lists of -interfast with the original shaders on them (same output)");
    say!(
        "  -interfastmode <n>    av1_set_inter_fast(n): 1 -interfast, +2 original shader for one reference, +4 for two;"
    );
    say!("                        8: only the room for the lists in the block maps, the path stays off (diagnostic)");
    say!("  -gputime              the timing run of the test player: GPU timestamps per stage and CPU timers, the");
    say!(
        "                        summary as 'GPUTIME ...' lines after the task (docs/test-player.md, \"Timing run\");"
    );
    say!("                        on WARP the times say nothing about a GPU, the counters are the stream's");
    say!("  -gputimedetail        -gputime with the inter prediction breakdown: a timestamp behind every section of");
    say!("                        the inter stage, work counters per section");
    say!(
        "  -gputimeisolate       -gputimedetail with a UAV barrier and a timestamp behind every dispatch of the inter"
    );
    say!("                        stage and of the reconstruction (same output)");
    say!("  -progress             with -gputime: one 'GPUTIME frame ...' line per frame behind the summary");
    say!("  -alllayers            a stream with spatial layers: output the frame of every layer of a temporal unit");
    say!("                        (default: one frame per temporal unit, the highest layer it has)");
    say!("  -oppoint <n>          a stream with layers: decode operating point n of its sequence header (default 0)");
    say!("Program flags:");
    say!("  -list <file>          one task per line (the task flags above; # starts a comment line)");
    say!("  -hw                   the first hardware adapter instead of WARP");
    say!("  -v                    print the decoder's log lines (MEMORY ...) and the player-style task lines");
    say!("  -debuglayer           switch the D3D12 debug layer on (needs the Graphics Tools feature of Windows)");
    say!("                        and print its messages after every task; -gpuvalidation adds GPU-based validation");
    say!("  -continue_on_error    a decode call that fails does not end the task: the error is counted and the next");
    say!("                        temporal units go to the same decoder, as an application does with a damaged");
    say!("                        stream (one 'CONTINUE ...' line per error and one behind the task)");
    say!("  -keep_going           a task that ends with a decode error does not end the run: the tasks behind it");
    say!("                        are run in the same process");
    say!("  -memstate             'MEMSTATE ...' lines with the memory of the process and the video memory the");
    say!("                        adapter reports, in bytes: before a task, with its decoder still there, and when");
    say!("                        everything of the task is released (testing/conformance/memory_check.py)");
    say!("  -parsedigest          no decode: the parser of phase 2 (rav1d's parse pass, docs/phase2-rav1d.md) on the");
    say!("                        stream of every task, one 'PARSEDIGEST ...' line per stream with what it read and");
    say!("                        a number that is the same on every platform; -t sets its threads. Needs no device");
    say!("  -capi                 use the decoder through its interface for C, the functions of rav1g.h, instead of");
    say!("                        the Rust interface: what an application that is not Rust links (same output)");
    say!(
        "  -sm6test              run the Shader Model 6 test shaders on the device before the tasks, one 'SM6TEST ...'"
    );
    say!("                        line per shader (docs/test-player.md); needs no stream: without -i and -list the");
    say!("                        program ends after the test");
    say!("  -playersmoke <dir>    run the decoder code of the test player (testing/player_common) through rav1g.h on");
    say!("                        the device, with the tasks of <dir>/args/args_av1.txt as the player reads them from");
    say!("                        its LocalState (streams relative to <dir>); the player's log goes to stdout. Exit");
    say!("                        code 0 when the player ended its launch (what its tasks did is in its log), 2 when");
    say!("                        it did not within 25 minutes. No other task runs with it");
    say!("");
    say!("Exit code: 0 all tasks decoded and matched, 1 a frame or a frame count differs, 2 a decode error (or");
    say!("-sm6test could not run its test), 3 bad arguments or no device, 4 timeout. One 'RESULT ...' line per task");
    say!("on stdout.");
}

/// A whole decimal number as C's `strtol(text, &end, 10)` with nothing behind it reads one: white space in
/// front, a sign, digits.
fn parse_int(text: &str) -> Option<i32> {
    let trimmed = text.trim_start_matches(|c: char| c == ' ' || ('\t'..='\r').contains(&c));
    if trimmed.is_empty() {
        return None;
    }
    let (negative, digits) = match trimmed.as_bytes()[0] {
        b'-' => (true, &trimmed[1..]),
        b'+' => (false, &trimmed[1..]),
        _ => (false, trimmed),
    };
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    // Out of range: strtol saturates at the limits of a 32-bit long, and the value is cut to an int.
    let magnitude = digits.parse::<i64>().unwrap_or(i64::MAX);
    let value = if negative { -magnitude } else { magnitude };
    Some(value.clamp(i32::MIN as i64, i32::MAX as i64) as i32)
}

/// Applies the arguments to the task (and to the globals when given).
pub fn parse_args(args: &[String], task: &mut Task, mut globals: Option<&mut Globals>) -> Result<(), String> {
    let mut i = 0;
    while i < args.len() {
        let a = args[i].as_str();
        let mut value = || -> Result<String, String> {
            if i + 1 >= args.len() {
                return Err(format!("{a} needs a value"));
            }
            i += 1;
            Ok(args[i].clone())
        };
        let mut number = || -> Result<i32, String> {
            let text = value()?;
            parse_int(&text).ok_or_else(|| format!("{a} needs a number, not '{text}'"))
        };
        match a {
            "-i" => task.input = value()?,
            "-name" => task.name = value()?,
            "-check" => task.check = value()?,
            "-md5_frame_check" => task.md5_frame_check = true,
            "-save_md5" => task.save_md5 = value()?,
            "-aom_md5" => task.aom_md5 = true,
            "-o" => task.out_yuv = value()?,
            "-save_wrong_yuv" => task.save_wrong_yuv = value()?,
            "-wrong_limit" => task.wrong_limit = number()?,
            "-dumpstages" => task.dumpstages = value()?,
            "-dumpdir" => task.dumpdir = value()?,
            "-limit" => task.limit = number()?,
            "-t" => task.threads = number()?,
            "-queue_size" => task.queue_size = number()?,
            "-timeout" => task.timeout_s = number()?,
            "-slots" => {
                task.slots = number()?;
                task.slots_given = true;
            }
            "-parseframes" => task.parse_frames = number()?,
            "-lowmemparts" => task.lowmem_parts = number()?,
            "-lowmem" => task.lowmem = true,
            "-budget" => task.budget_mib = i64::from(number()?),
            "-cdefv1" => task.cdefv1 = true,
            "-cdefv2" => task.cdefv2 = true,
            "-nofitsize" => task.nofitsize = true,
            "-fitsize" => task.fitsize = true,
            "-intrav2" => task.intrav2 = true,
            "-intraopt" => task.intraopt = true,
            "-gpuclear" => task.clears = Clears::Gpu,
            "-cpuclear" | "-asyncclear" => task.clears = Clears::CompletionThread,
            "-syncclear" => task.clears = Clears::DecodeThread,
            "-lfhsplit" => task.lfhsplit = number()?,
            "-intraedgefix" => task.intraedgefix = true,
            "-continue_on_error" => task.continue_on_error = true,
            "-poisonmi" => task.poison = Poison::ModeInfo,
            "-poisonbuf" => task.poison = Poison::Buffers,
            "-interfast" => task.interfast = switches::INTER_FAST_ON,
            "-interfastlists" => task.interfast = switches::INTER_FAST_LISTS_ONLY,
            "-interfastmode" => task.interfast = number()? as u32,
            "-gputime" => task.gputime = true,
            "-gputimedetail" => {
                task.gputime = true;
                if task.gputime_detail < 1 {
                    task.gputime_detail = 1;
                }
            }
            "-gputimeisolate" => {
                task.gputime = true;
                task.gputime_detail = 2;
            }
            "-progress" => task.progress = true,
            "-alllayers" => task.all_layers = true,
            "-oppoint" => task.oppoint = number()?,
            "-parser" => {
                task.parser_rav1d = match value()?.as_str() {
                    "c" => false,
                    "rav1d" => true,
                    other => return Err(format!("-parser {other}: c or rav1d")),
                }
            }
            "-parseopt" => {
                let list = value()?;
                task.parser_options = rav1g::ParserOptions::from_words(&list).ok_or_else(|| {
                    format!("-parseopt {list}: nolfmask, tilethread, time, latelists, cleargrid, default or none")
                })?;
            }
            _ => {
                let Some(globals) = globals.as_deref_mut() else {
                    return Err(format!("unknown argument '{a}'"));
                };
                match a {
                    "-hw" => globals.hardware = true,
                    "-v" => globals.verbose = true,
                    "-debuglayer" => globals.debug_layer = true,
                    "-gpuvalidation" => {
                        globals.debug_layer = true;
                        globals.gpu_validation = true;
                    }
                    "-sm6test" => globals.sm6test = true,
                    "-capi" => globals.capi = true,
                    "-keep_going" => globals.keep_going = true,
                    "-memstate" => globals.memstate = true,
                    "-parsedigest" => globals.parse_digest = true,
                    "-list" => globals.list = value()?,
                    "-playersmoke" => globals.player_smoke = value()?,
                    _ => return Err(format!("unknown argument '{a}'")),
                }
            }
        }
        i += 1;
    }
    Ok(())
}

/// The checks the player's parser makes after reading a task line.
pub fn check_task(task: &mut Task) -> Result<(), String> {
    if task.input.is_empty() {
        return Err("no stream (-i)".to_string());
    }
    if task.cdefv1 && task.cdefv2 {
        return Err("-cdefv1 and -cdefv2 cannot be used together".to_string());
    }
    if task.nofitsize && task.fitsize {
        return Err("-nofitsize and -fitsize cannot be used together".to_string());
    }
    if task.slots != 0 && (task.slots < SLOTS_MIN || task.slots > SLOTS_MAX) {
        return Err(format!(
            "-slots {} is not allowed: the decoder takes {} to {} slots (0: the library's default)",
            task.slots, SLOTS_MIN, SLOTS_MAX
        ));
    }
    if task.parse_frames < 0 || task.parse_frames > PARSE_FRAMES_MAX {
        return Err(format!(
            "-parseframes {} is not allowed: 1 to {} frames in parse (0: the library's default)",
            task.parse_frames, PARSE_FRAMES_MAX
        ));
    }
    // More than one frame in parse wants more slots than the two of this program's default: without
    // -slots the library says how many.
    if !task.slots_given && task.parse_frames != 1 {
        task.slots = 0;
    }
    if task.lowmem_parts < 0 || task.lowmem_parts > LOWMEM_ALL {
        return Err(format!(
            "-lowmemparts {} is not allowed: the sum of 1, 2 and 4, at most {}",
            task.lowmem_parts, LOWMEM_ALL
        ));
    }
    if task.lowmem {
        task.lowmem_parts = LOWMEM_ALL;
    }
    if task.lfhsplit < 0 || task.lfhsplit > 2 {
        return Err(format!("-lfhsplit {} is not allowed: 0, 1 or 2", task.lfhsplit));
    }
    if task.queue_size < 1 || task.queue_size > QUEUE_SIZE_MAX {
        return Err(format!("-queue_size {} is not allowed: 1 to {}", task.queue_size, QUEUE_SIZE_MAX));
    }
    if task.threads < 0 {
        task.threads = 1;
    }
    if !task.dumpstages.is_empty() {
        match shim::parse_dump_frames(&task.dumpstages) {
            Some(frames) => task.dump_frames = frames,
            None => {
                return Err(format!(
                    "-dumpstages {} is not a frame list like 0,3,10-12 of at most 256 frames",
                    task.dumpstages
                ));
            }
        }
    }
    if task.md5_frame_check && task.check.is_empty() {
        task.check = format!("{}.md5", task.input);
    }
    if task.name.is_empty() {
        task.name = match task.input.rfind(['\\', '/']) {
            Some(position) => task.input[position + 1..].to_string(),
            None => task.input.clone(),
        };
    }
    // The name is one token of the result line.
    task.name = task.name.chars().map(|c| if c == ' ' || c == '\t' || c == '"' { '_' } else { c }).collect();
    Ok(())
}

/// A line of a list file into arguments: split at white space, double quotes keep a value together.
pub fn split_line(line: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    let mut quoted = false;
    let mut have = false;
    for c in line.chars() {
        if c == '"' {
            quoted = !quoted;
            have = true;
        } else if !quoted && (c == ' ' || c == '\t' || c == '\r' || c == '\n') {
            if have {
                out.push(std::mem::take(&mut current));
            }
            current.clear();
            have = false;
        } else {
            current.push(c);
            have = true;
        }
    }
    if have {
        out.push(current);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strings(args: &[&str]) -> Vec<String> {
        args.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn numbers_as_strtol() {
        assert_eq!(parse_int("12"), Some(12));
        assert_eq!(parse_int("-3"), Some(-3));
        assert_eq!(parse_int("+7"), Some(7));
        assert_eq!(parse_int(" 5"), Some(5));
        assert_eq!(parse_int(""), None);
        assert_eq!(parse_int("5x"), None);
        assert_eq!(parse_int("5 "), None);
        assert_eq!(parse_int("-"), None);
        assert_eq!(parse_int("99999999999"), Some(i32::MAX));
    }

    #[test]
    fn list_lines() {
        assert_eq!(split_line("-i a.ivf  -name\tb\r"), strings(&["-i", "a.ivf", "-name", "b"]));
        assert_eq!(split_line("-i \"a b.ivf\" -name \"\""), strings(&["-i", "a b.ivf", "-name", ""]));
        assert_eq!(split_line("   "), Vec::<String>::new());
    }

    #[test]
    fn a_task_of_the_conformance_manifest() {
        let mut task = Task::default();
        let mut globals = Globals::default();
        let line = "-name av1-1-b8-01-size-16x16.ivf -i aom/av1-1-b8-01-size-16x16.ivf -check \
                    aom/av1-1-b8-01-size-16x16.ivf.md5 -aom_md5 -timeout 300 -save_md5 \"out dir/x.md5\"";
        parse_args(&split_line(line), &mut task, None).unwrap();
        parse_args(&strings(&["-list", "tasks.list", "-t", "4", "-interfast"]), &mut task, Some(&mut globals)).unwrap();
        assert_eq!(task.name, "av1-1-b8-01-size-16x16.ivf");
        assert_eq!(task.save_md5, "out dir/x.md5");
        assert_eq!((task.threads, task.timeout_s, task.interfast, task.aom_md5), (4, 300, 1, true));
        assert_eq!(globals.list, "tasks.list");
        assert_eq!(task.poison, Poison::Off);
        parse_args(&strings(&["-poisonmi"]), &mut task, None).unwrap();
        assert_eq!(task.poison, Poison::ModeInfo);
        parse_args(&strings(&["-poisonbuf"]), &mut task, None).unwrap();
        assert_eq!(task.poison, Poison::Buffers);
        // The clears of a slot: nothing to clear without a flag.
        assert_eq!(task.clears, Clears::NotNeeded);
        for (flag, clears) in [
            ("-gpuclear", Clears::Gpu),
            ("-cpuclear", Clears::CompletionThread),
            ("-syncclear", Clears::DecodeThread),
            ("-asyncclear", Clears::CompletionThread),
        ] {
            parse_args(&strings(&[flag]), &mut task, None).unwrap();
            assert_eq!(task.clears, clears, "{flag}");
        }
        // A program flag is not a task flag of a list line.
        assert!(parse_args(&strings(&["-hw"]), &mut task, None).is_err());
        assert!(parse_args(&strings(&["-t"]), &mut task, None).is_err());
        assert!(parse_args(&strings(&["-t", "four"]), &mut task, None).is_err());
    }

    #[test]
    fn the_name_is_the_file_name() {
        let mut task = Task { input: "dir\\sub/a clip.ivf".to_string(), md5_frame_check: true, ..Task::default() };
        // Not on a path that needs the frame list parser.
        check_task(&mut task).unwrap();
        assert_eq!(task.name, "a_clip.ivf");
        assert_eq!(task.check, "dir\\sub/a clip.ivf.md5");
        let mut bad = Task { input: "a.ivf".to_string(), slots: 5, ..Task::default() };
        assert!(check_task(&mut bad).is_err());
    }
}
