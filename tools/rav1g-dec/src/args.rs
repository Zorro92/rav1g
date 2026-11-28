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
use rav1g::switches::Poison;

// The player's values (testing/player_common/cmd_parser.h, decoder_creator.h).
pub const SLOTS_MIN: i32 = 2;
pub const SLOTS_MAX: i32 = 4;
/// -parseframes: the most frames a decoder parses at the same time.
pub const PARSE_FRAMES_MAX: i32 = 4;
pub const LOWMEM_ALL: i32 = 7;
/// -lowmemparts: every part there is: those of -lowmem, and 32, the frame buffer bound at the recording
/// (`LowMemory::LATE_FRAME_BUFFER`), which -lowmem does not have.
pub const LOWMEM_PARTS: i32 = 39;
/// What a task without -lowmemparts and -lowmem has: the library's default for the decoder's size
/// (`LowMemory::DEFAULT`, `RAV1G_LOW_MEMORY_DEFAULT`).
pub const LOWMEM_DEFAULT: i32 = 16;
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
    pub annexb: bool,
    /// -name, or the file name of the input: what the result line calls the stream.
    pub name: String,
    /// -check: reference md5 file.
    pub check: String,
    /// -check_stream: one MD5 of the concatenated visible output frames.
    pub check_stream: String,
    pub skip_film_grain: bool,
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
    /// -output: what the output images are.
    pub output: crate::pool::ImageMode,
    /// -sharedfence: the output fence is created shareable, and opened through a handle.
    pub shared_fence: bool,
    /// -t: the threads that parse; 0, which a task without -t has, is the library's default for the
    /// frame size.
    pub threads: i32,
    /// -parseframes: the frames whose parse may run at the same time; 0, which a task without
    /// -parseframes has, is the library's default for the frame size.
    pub parse_frames: i32,
    pub limit: i32,
    pub queue_size: i32,
    /// -slots: the pipeline slots; 0, which a task without -slots has, is the library's default (one
    /// more than frames in parse).
    pub slots: i32,
    /// The task has -slots.
    pub slots_given: bool,
    /// -displaypass WxH: the display pass (`Settings::display`); (0, 0) off. -displaypeak, -displaywhite
    /// (nits, 0 the default), -displaydepth (8, 10, 0 the decoder's), -displaytransform (0 by the
    /// frame's colour, 1 scale, 2 gamut, 3 PQ, 4 HLG), -displaydither (0 auto: NV12 dithered, 1 off, 2
    /// ordered at either depth), -displaymaxcll and -displaymasteringmax (the application's HDR metadata,
    /// nits, 0 none), -displayfixedpeak and -displaybilinearchroma (the A/B switches of av1_perf.h).
    pub displaypass: (i32, i32),
    pub displaypeak: f32,
    pub displaywhite: f32,
    pub displaydepth: u32,
    pub displaytransform: u32,
    pub displaydither: u32,
    pub displaymaxcll: f32,
    pub displaymasteringmax: f32,
    pub displayfixedpeak: bool,
    pub displaybilinearchroma: bool,
    pub nofitsize: bool,
    pub fitsize: bool,
    pub lowmem: bool,
    pub lowmem_parts: i32,
    /// -budget: what the decoder is told the application has left, in MiB; negative: no budget callback.
    pub budget_mib: i64,
    /// -headroom: what the budget guard should leave of the budget behind the decoder, in MiB; 0: the
    /// library's default.
    pub headroom_mib: i64,
    /// -poisonmi, -poisonbuf: `switches::set_poison_buffers`.
    pub poison: Poison,
    /// -continue_on_error: a decode call that fails does not end the task; the next temporal units go to
    /// the same decoder.
    pub continue_on_error: bool,
    /// -anystart: the task starts with the first temporal unit of the stream, not with the first one that
    /// has a key frame, as an application does that starts in the middle of a stream (AV1 7.6): the
    /// decoder is created for the first sequence header, and the temporal units before it are passed too.
    pub any_start: bool,
    /// -flush <list>: the decoder is flushed (`Decoder::flush`, `rav1g_decoder_flush`) before it gets
    /// each of these temporal units (counted from 0, the first the decoder gets; a number twice flushes
    /// twice in a row; one at or past the end flushes behind the last unit, before the drain).
    pub flush: Vec<i32>,
    /// -flushdrain: a drain before each flush, whose errors count as part of the seek.
    pub flush_drain: bool,
    /// -flushrefuse: from the start of each flush (its drain, with -flushdrain) until it has returned,
    /// the output refuses every image (`get_image` returns non-zero), as an application that seeks does.
    pub flush_refuse: bool,
    /// -refuse <list>: the output refuses every image during the decode calls of these temporal units
    /// (counted as for -flush): the frame is decoded and not output (with -continue_on_error the task
    /// goes on behind the error the call returns).
    pub refuse: Vec<i32>,
    /// -alllayers: every shown frame of a temporal unit with spatial layers.
    pub all_layers: bool,
    /// -oppoint <n>: the operating point of a stream with layers.
    pub oppoint: i32,
    /// -parseopt <list>: the switches of rav1d's parse pass (`rav1g::ParserOptions`).
    pub parser_options: rav1g::ParserOptions,
    /// -timeout: seconds for the task, 0 = none.
    pub timeout_s: i32,
    /// -gputime: the timing run of the player, its summary on stdout.
    pub gputime: bool,
    /// -gputimedetail (1), -gputimeisolate (2): the inter breakdown; either sets gputime.
    pub gputime_detail: i32,
    /// -progress: with -gputime, the per-frame lines behind the summary; with -memstats, a line a frame.
    pub progress: bool,
    /// -memstats: the decoder's memory statistics, `MEMSTATS` lines behind the drain (docs/memory.md).
    pub memstats: bool,
    /// -dumpstages <frame list>
    pub dumpstages: String,
    /// -dumpdir <dir>
    pub dumpdir: String,
    /// The frames of -dumpstages.
    pub dump_frames: Vec<i32>,
    /// -intrasteptime <frame list>: the frames whose intra steps are timed.
    pub intrasteptime: String,
    pub step_frames: Vec<i32>,
    /// -intrastepmin <n>: also time every frame with more than n intra steps (-1: none).
    pub step_min: i32,
}

impl Default for Task {
    fn default() -> Task {
        Task {
            input: String::new(),
            annexb: false,
            name: String::new(),
            check: String::new(),
            check_stream: String::new(),
            skip_film_grain: false,
            md5_frame_check: false,
            save_md5: String::new(),
            out_yuv: String::new(),
            save_wrong_yuv: String::new(),
            wrong_limit: -1,
            aom_md5: false,
            output: crate::pool::ImageMode::Planes,
            shared_fence: false,
            threads: 0,
            limit: -1,
            queue_size: QUEUE_SIZE_DEFAULT,
            slots: 0,
            slots_given: false,
            parse_frames: 0,
            displaypass: (0, 0),
            displaypeak: 0.0,
            displaywhite: 0.0,
            displaydepth: 0,
            displaytransform: 0,
            displaydither: 0,
            displaymaxcll: 0.0,
            displaymasteringmax: 0.0,
            displayfixedpeak: false,
            displaybilinearchroma: false,
            nofitsize: false,
            fitsize: false,
            lowmem: false,
            lowmem_parts: LOWMEM_DEFAULT,
            budget_mib: -1,
            headroom_mib: 0,
            poison: Poison::Off,
            continue_on_error: false,
            any_start: false,
            flush: Vec::new(),
            flush_drain: false,
            flush_refuse: false,
            refuse: Vec::new(),
            all_layers: false,
            parser_options: rav1g::ParserOptions::default(),
            oppoint: 0,
            timeout_s: 0,
            gputime: false,
            gputime_detail: 0,
            progress: false,
            memstats: false,
            dumpstages: String::new(),
            dumpdir: "dumps".to_string(),
            dump_frames: Vec::new(),
            intrasteptime: String::new(),
            step_frames: Vec::new(),
            step_min: -1,
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
    /// -shaderpick <pick>
    pub shader_pick: String,
    /// -prepare <list>: the pipeline states the device prepares; `None` is all of them.
    pub prepare: Option<rav1g::Prepare>,
    /// -preparetest <mode>: `rav1g::switches::set_pipeline_prepare_test`.
    pub prepare_test: u32,
}

/// -prepare <list>: words separated by commas, `RAV1G_PREPARE_ONLY` with their bits: 8, 10 (the bit
/// depths), sdr, pq, hlg (the display pass), grain, superres, scaled; `core` for none of them (the states
/// every stream needs only); `all` for every state.
pub fn parse_prepare(list: &str) -> Result<rav1g::Prepare, String> {
    use rav1g::Prepare;
    if list == "all" {
        return Ok(Prepare::ALL);
    }
    let mut prepare = Prepare::ONLY;
    for word in list.split(',') {
        prepare = prepare
            | match word {
                "8" => Prepare::BIT_DEPTH_8,
                "10" => Prepare::BIT_DEPTH_10,
                "sdr" => Prepare::DISPLAY_SDR,
                "pq" => Prepare::DISPLAY_PQ,
                "hlg" => Prepare::DISPLAY_HLG,
                "grain" => Prepare::FILM_GRAIN,
                "superres" => Prepare::SUPERRES,
                "scaled" => Prepare::SCALED_REFERENCES,
                "core" => Prepare::ONLY,
                _ => {
                    return Err(format!(
                        "-prepare {list}: all, or a list of 8, 10, sdr, pq, hlg, grain, superres, scaled, core"
                    ));
                }
            };
    }
    Ok(prepare)
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
    say!("  -annexb               input is a length-delimited AV1 Annex B stream");
    say!("  -name <text>          what the BEGIN and RESULT lines call the stream (default: its file name)");
    say!("  -check <file>         compare with this md5 file: the first 32 characters of each line are the md5 of");
    say!("                        one output frame (the player's format and that of AOM's test vectors)");
    say!("  -md5_frame_check      the same with <stream>.md5, as in the player");
    say!("  -check_stream <file>  compare one MD5 of all visible frames, in output order (Argon Streams)");
    say!("  -skip-film-grain      disable film grain synthesis");
    say!("  -save_md5 <file>      write the md5 of every output frame in the player's format");
    say!("  -aom_md5              hash a monochrome frame with two grey chroma planes, as aomdec --i420 does");
    say!("  -output <kind>        the output images (docs/output.md): planes (the default: a buffer with the");
    say!("                        planes), p010 (the semi-planar output: an NV12 texture for 8-bit and a P010");
    say!("                        texture for 10-bit frames, which shaders may write; nv12 is the same word),");
    say!("                        p010-copy (a texture shaders may not write), p010-buffer (the two planes in a");
    say!("                        buffer). The textures are read back and hashed as the planes are.");
    say!("  -sharedfence          the output fence is created shareable and opened through a shared handle");
    say!("                        (AOM's .md5 files); without it luma only, as the player and dav1d do");
    say!("  -o <file>             write the visible samples of every output frame (planar, as they are hashed)");
    say!("  -save_wrong_yuv <dir> with a reference: write each frame that differs to <dir>/<name>_<frame>.yuv");
    say!("  -wrong_limit <n>      -save_wrong_yuv writes at most n frames per task");
    say!("  -dumpstages <frames>  write the buffers behind every GPU stage of these frames (decoding order from");
    say!("                        0, hidden frames count; a list like 0,3,10-12) and the lists the stages read");
    say!("                        (mode info, block records, loop filter edge lists) to <dir>/<name>_f<n>.stages,");
    say!("                        as the player does; compare two runs with testing/stage_dump/stage_dump.py");
    say!("  -dumpdir <dir>        where -dumpstages writes (default dumps; created when it is not there)");
    say!("  -intrasteptime <frames>");
    say!("                        a GPU timestamp behind every intra wavefront step of these frames (decoding order,");
    say!("                        from 0, like 0,3,10-12; the no= of 'GPUTIME frame' lines), with what each step");
    say!("                        holds: one 'STEPTIME frame' line per frame and one 'STEPTIME step' line per step");
    say!("                        (docs/test-player.md, \"Intra step timing\"; same output)");
    say!("  -intrastepmin <n>     the same for every frame with more than n intra steps (with -intrasteptime: both)");
    say!("  -limit <n>            decode only the first n temporal units of the stream");
    say!("  -t <n>                decoder threads: the tiles of a frame and, with more than one frame in");
    say!("                        parse, the frames. Without it, or 0: the library's default for the frame");
    say!("                        size, 1 up to 2560x1440 and 2 above");
    say!("  -parseframes <n>      frames whose parse may run at the same time, 1..4 and at most the threads.");
    say!("                        Without it, or 0: the library's default for the frame size, 1 up to");
    say!("                        2560x1440 and 2 above. With more than 1 the frames are parsed side by side");
    say!("                        and submitted in decode order by a thread of the decoder");
    say!("                        (docs/phase2-rav1d.md, \"Frames in parallel\"); the output is the same.");
    say!("                        Without -slots the decoder has one slot more than frames in parse, or what");
    say!("                        its budget guard leaves it");
    say!("  -queue_size <n>       output images in the pool, 1..{} (default {})", QUEUE_SIZE_MAX, QUEUE_SIZE_DEFAULT);
    say!("  -timeout <seconds>    give up on a task after this time (exit code {})", EXIT_TIMEOUT);
    say!("  -nofitsize -lowmemparts <n> -lowmem -slots <n>");
    say!(
        "  -displaypass WxH      the display pass (Rav1gSettings::display): images of WxH, the frames downscaled and \
         tone-mapped to BT.709 SDR, P010 or NV12 (with -output p010; the md5 references do not apply)"
    );
    say!(
        "  -displaypeak N -displaywhite N -displaydepth 8|10 -displaytransform 0-4   its source peak and SDR white \
         (nits), image depth, transform (0 by the frame's colour, 1 scale, 2 gamut, 3 PQ, 4 HLG)"
    );
    say!(
        "  -displaydither 0-2   the display pass's last quantisation: 0 the ordered dither for NV12 (default), \
         1 rounded, 2 the ordered dither for P010 too"
    );
    say!(
        "  -displaymaxcll N -displaymasteringmax N   the application's HDR metadata (nits, 0 none): the PQ \
         source peak takes them where the stream's metadata says nothing"
    );
    say!(
        "  -displayfixedpeak -displaybilinearchroma   A/B: the configured peak whatever the metadata; the \
         bilinear input chroma (before interface 3)"
    );
    say!("                        as in the test player (docs/test-player.md); -cdefv2, -intraopt and -fitsize");
    say!("                        (the defaults) are accepted");
    say!("  -budget <MiB>         the decoder is told the application has this much memory left for it: the budget");
    say!("                        guard of the creation (MEMORY budget and MEMORY REFUSED lines, with -v)");
    say!("  -headroom <MiB>       with -budget: what the guard should leave of it behind the decoder; a decoder");
    say!("                        that would leave less is created with fewer slots and frames in parse (MEMORY");
    say!("                        headroom lines). Default 0: the library's 128; 48 or less is the guard's margin");
    say!("  -poisonmi             debugging: the mode info pool starts filled with the byte 0xA5 instead of with what");
    say!("                        the memory held, so a shader that reads bytes the decoder never wrote shows in");
    say!("                        the output of any stream (same output)");
    say!("  -poisonbuf            the same for every buffer the CPU writes for the shaders, and the constants of");
    say!("                        every stage filled again in every frame before the stage writes them (same output)");
    say!("  -parseopt <list>      the switches of rav1d's parse, separated by commas (same output). nolfmask: rav1d");
    say!("                        leaves out the masks of its own loop filter; tilethread: a tile stays on its");
    say!("                        thread between its superblock rows; gpucdef: the two lists CDEF reads are written");
    say!("                        by a pass of the GPU (cdef_lists) and not by the thread that records the frame.");
    say!("                        gpugrid: the GPU builds the mode info grid and writes the CDEF lists. These four");
    say!("                        are on by default. Naming any of them, none or default selects them explicitly.");
    say!("                        time: with -gputime, the calls of the sink are timed (time alone: the default,");
    say!("                        timed). -parseopt nolfmask,tilethread,gpucdef keeps the sink-written grid.");
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
    say!("  -memstats             memory statistics (docs/memory.md): 'MEMSTATS ...' lines behind the drain: what the");
    say!("                        frames used of the pools and buffers that hold a worst case, the frame buffers out");
    say!("                        of their pool, the bytes the CPU writes for a frame; with -progress a line for");
    say!("                        every frame (same output)");
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
    say!("  -anystart             the task starts with the first temporal unit, not the first key frame: the decoder");
    say!("                        is created for the first sequence header and given every temporal unit (a start");
    say!("                        in the middle of a stream, AV1 7.6; frames without decoded references are skipped)");
    say!("  -flush <list>         flush the decoder (rav1g_decoder_flush, a seek) before it gets these temporal units");
    say!("                        (from 0, the first it gets; twice in a row with a number twice; at or past the end");
    say!("                        behind the last one): a 'FLUSH' line says how many frames came out before it, and a");
    say!("                        frame of before the flush that comes out after it is a fault. The stream goes on");
    say!("                        with the unit behind (testing/conformance/flush_check.py writes streams that jump)");
    say!("  -flushdrain           a drain before each flush (its errors are part of the seek), as an application that");
    say!("                        drains first does");
    say!("  -flushrefuse          every image is refused from the start of each flush (its drain) until it returns,");
    say!("                        as an application that seeks does (Rav1gOutput::get_image returns non-zero)");
    say!("  -refuse <list>        every image is refused in the decode calls of these temporal units (from 0): their");
    say!("                        frames are decoded and not output (with -continue_on_error the task goes on)");
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
    say!("  -shaderpick <pick>    the shader-pick build takes od=<list>,dxc=<list> to choose pipeline");
    say!("                        bytecode per shader. dxc is the default DXC build, od uses DXC with -Od.");
    say!("                        Without the flag every shader uses the dxc set. A list is");
    say!("                        shader names separated by commas, 'all', or names with '*'; a later part overrides");
    say!("                        an earlier one (od=all,dxc=loopfilter_vert_hbd). 'SHADERSET ...' and 'SHADERPICK");
    say!("                        ...' lines say what was taken (docs/test-player.md, \"Shader pick\")");
    say!("  -prepare <list>       the pipeline states the device compiles at its creation (Rav1gDeviceSettings::");
    say!("                        prepare): 'all' (the default), or words separated by commas: 8, 10 (bit depths),");
    say!("                        sdr, pq, hlg (display pass), grain, superres, scaled, core (none of these). The");
    say!("                        others are compiled when a decoder needs them ('PIPELINE ...' lines; docs/");
    say!("                        integration.md, \"Preparing the pipeline states\"). The output is the same");
    say!("  -preparetest <mode>   the test of the on-demand states: 1 the device prepares nothing (a decoder creation");
    say!("                        or a frame compiles what it needs), 2 nothing is prepared anywhere (every state at");
    say!("                        its first dispatch). The output is the same");
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
            "-annexb" => task.annexb = true,
            "-name" => task.name = value()?,
            "-check" => task.check = value()?,
            "-check_stream" => task.check_stream = value()?,
            "-skip-film-grain" => task.skip_film_grain = true,
            "-md5_frame_check" => task.md5_frame_check = true,
            "-save_md5" => task.save_md5 = value()?,
            "-aom_md5" => task.aom_md5 = true,
            "-output" => {
                task.output = crate::pool::ImageMode::parse(&value()?)
                    .ok_or_else(|| "-output takes planes, p010 (or nv12), p010-copy or p010-buffer".to_string())?;
            }
            "-sharedfence" => task.shared_fence = true,
            "-o" => task.out_yuv = value()?,
            "-save_wrong_yuv" => task.save_wrong_yuv = value()?,
            "-wrong_limit" => task.wrong_limit = number()?,
            "-dumpstages" => task.dumpstages = value()?,
            "-intrasteptime" => task.intrasteptime = value()?,
            "-intrastepmin" => task.step_min = number()?,
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
            "-headroom" => task.headroom_mib = i64::from(number()?),
            // The default since the A/B flags of CDEF are retired; accepted, changes nothing.
            "-cdefv2" => {}
            "-displaypass" => {
                let text = value()?;
                let size = text.split_once('x').and_then(|(w, h)| Some((w.parse().ok()?, h.parse().ok()?)));
                task.displaypass = size.ok_or_else(|| format!("-displaypass {text}: WxH"))?;
            }
            "-displaypeak" | "-displaywhite" | "-displaymaxcll" | "-displaymasteringmax" => {
                let text = value()?;
                let nits: f32 = text.parse().map_err(|_| format!("{a} {text}: nits"))?;
                match a {
                    "-displaypeak" => task.displaypeak = nits,
                    "-displaywhite" => task.displaywhite = nits,
                    "-displaymaxcll" => task.displaymaxcll = nits,
                    _ => task.displaymasteringmax = nits,
                }
            }
            "-displayfixedpeak" => task.displayfixedpeak = true,
            "-displaybilinearchroma" => task.displaybilinearchroma = true,
            "-displaydepth" => task.displaydepth = number()? as u32,
            "-displaytransform" => task.displaytransform = number()? as u32,
            "-displaydither" => task.displaydither = number()? as u32,
            "-nofitsize" => task.nofitsize = true,
            "-fitsize" => task.fitsize = true,
            // The default since -intraod is retired; accepted, changes nothing.
            "-intraopt" => {}
            "-continue_on_error" => task.continue_on_error = true,
            "-anystart" => task.any_start = true,
            "-flush" | "-refuse" => {
                let text = value()?;
                let list: Option<Vec<i32>> =
                    text.split(',').map(|unit| unit.trim().parse().ok().filter(|&unit: &i32| unit >= 0)).collect();
                let mut list = list.ok_or_else(|| format!("{a} {text}: temporal unit numbers like 0,0,12"))?;
                list.sort_unstable();
                if a == "-flush" { task.flush = list } else { task.refuse = list }
            }
            "-flushdrain" => task.flush_drain = true,
            "-flushrefuse" => task.flush_refuse = true,
            "-poisonmi" => task.poison = Poison::ModeInfo,
            "-poisonbuf" => task.poison = Poison::Buffers,
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
            "-memstats" => task.memstats = true,
            "-alllayers" => task.all_layers = true,
            "-oppoint" => task.oppoint = number()?,
            "-parser" => {
                return Err(String::from(
                    "-parser is gone: the parse pass of rav1d reads every stream; the entropy decoder of libaom \
                     (-parser c) left the decoder (docs/phase2-rav1d.md, step 6)",
                ));
            }
            "-parseopt" => {
                let list = value()?;
                task.parser_options = rav1g::ParserOptions::from_words(&list).ok_or_else(|| {
                    format!(
                        "-parseopt {list}: nolfmask, tilethread, gpucdef, time, gpugrid, \
                         default or none"
                    )
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
                    "-shaderpick" => globals.shader_pick = value()?,
                    "-prepare" => globals.prepare = Some(parse_prepare(&value()?)?),
                    "-preparetest" => {
                        let mode = value()?;
                        globals.prepare_test = match mode.as_str() {
                            "0" | "1" | "2" => mode.parse().unwrap_or(0),
                            _ => return Err(format!("-preparetest {mode}: 0, 1 or 2")),
                        };
                    }
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
    // Without -slots the library says how many: one more than the frames in parse.
    if !task.slots_given {
        task.slots = 0;
    }
    if task.lowmem_parts != LOWMEM_DEFAULT && (task.lowmem_parts < 0 || task.lowmem_parts & !LOWMEM_PARTS != 0) {
        return Err(format!(
            "-lowmemparts {} is not allowed: the sum of 1, 2, 4 and 32, at most {}",
            task.lowmem_parts, LOWMEM_PARTS
        ));
    }
    if task.lowmem {
        task.lowmem_parts = LOWMEM_ALL;
    }
    if task.queue_size < 1 || task.queue_size > QUEUE_SIZE_MAX {
        return Err(format!("-queue_size {} is not allowed: 1 to {}", task.queue_size, QUEUE_SIZE_MAX));
    }
    if task.threads < 0 {
        task.threads = 0;
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
    if !task.intrasteptime.is_empty() {
        match shim::parse_dump_frames(&task.intrasteptime) {
            Some(frames) => task.step_frames = frames,
            None => {
                return Err(format!(
                    "-intrasteptime {} is not a frame list like 0,3,10-12 of at most 256 frames",
                    task.intrasteptime
                ));
            }
        }
    }
    if task.step_min < -1 {
        task.step_min = -1;
    }
    if !task.check_stream.is_empty() {
        if !task.check.is_empty() || task.md5_frame_check {
            return Err("-check_stream and per-frame comparisons are mutually exclusive".to_string());
        }
        if task.limit >= 0 {
            return Err("-check_stream requires the whole stream".to_string());
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
        parse_args(&strings(&["-list", "tasks.list", "-t", "4", "-nofitsize"]), &mut task, Some(&mut globals)).unwrap();
        assert_eq!(task.name, "av1-1-b8-01-size-16x16.ivf");
        assert_eq!(task.save_md5, "out dir/x.md5");
        assert_eq!((task.threads, task.timeout_s, task.nofitsize, task.aom_md5), (4, 300, true, true));
        assert_eq!(globals.list, "tasks.list");
        assert_eq!(task.poison, Poison::Off);
        parse_args(&strings(&["-poisonmi"]), &mut task, None).unwrap();
        assert_eq!(task.poison, Poison::ModeInfo);
        parse_args(&strings(&["-poisonbuf"]), &mut task, None).unwrap();
        assert_eq!(task.poison, Poison::Buffers);
        // The retired switches are unknown flags now; the flags of a default are accepted.
        for flag in [
            "-gpuclear",
            "-cpuclear",
            "-syncclear",
            "-asyncclear",
            "-intraod",
            "-intraedgefix",
            "-cdefv1",
            "-intrav2",
            "-interfast",
            "-interfastlists",
            "-interfastmode",
        ] {
            assert!(parse_args(&strings(&[flag]), &mut task, None).is_err(), "{flag}");
        }
        for flag in ["-cdefv2", "-intraopt", "-fitsize"] {
            parse_args(&strings(&[flag]), &mut task, None).unwrap();
        }
        // A program flag is not a task flag of a list line.
        assert!(parse_args(&strings(&["-hw"]), &mut task, None).is_err());
        assert!(parse_args(&strings(&["-t"]), &mut task, None).is_err());
        assert!(parse_args(&strings(&["-t", "four"]), &mut task, None).is_err());
    }

    #[test]
    fn the_low_memory_parts() {
        let parts = |text: &str, lowmem: bool| {
            let mut task = Task { input: "a.ivf".to_string(), ..Task::default() };
            parse_args(&strings(&["-lowmemparts", text]), &mut task, None).unwrap();
            task.lowmem = lowmem;
            check_task(&mut task).map(|()| task.lowmem_parts)
        };
        // The sum of 1, 2, 4 and 32 (the frame buffer bound at the recording); 16 is the default.
        for allowed in [0, 1, 3, 7, 32, 33, 35, 39, LOWMEM_DEFAULT] {
            assert_eq!(parts(&allowed.to_string(), false), Ok(allowed));
        }
        for refused in [-1, 8, 15, 17, 40, 48, 64] {
            assert!(parts(&refused.to_string(), false).is_err(), "{refused}");
        }
        // -lowmem is the three parts it always was.
        assert_eq!(parts("32", true), Ok(LOWMEM_ALL));
        assert_eq!(LOWMEM_PARTS as u32, (rav1g::LowMemory::ALL | rav1g::LowMemory::LATE_FRAME_BUFFER).0);
    }

    #[test]
    fn the_prepare_lists() {
        use rav1g::Prepare;
        assert_eq!(parse_prepare("all"), Ok(Prepare::ALL));
        assert_eq!(parse_prepare("core"), Ok(Prepare::ONLY));
        assert_eq!(parse_prepare("10,pq"), Ok(Prepare::ONLY | Prepare::BIT_DEPTH_10 | Prepare::DISPLAY_PQ));
        assert_eq!(
            parse_prepare("8,10,sdr,hlg,grain,superres,scaled"),
            Ok(Prepare::ONLY
                | Prepare::BIT_DEPTH_8
                | Prepare::BIT_DEPTH_10
                | Prepare::DISPLAY_SDR
                | Prepare::DISPLAY_HLG
                | Prepare::FILM_GRAIN
                | Prepare::SUPERRES
                | Prepare::SCALED_REFERENCES)
        );
        assert!(parse_prepare("10,hdr").is_err());
        assert!(parse_prepare("").is_err());
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
