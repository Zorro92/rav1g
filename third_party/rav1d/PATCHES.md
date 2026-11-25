# rav1d in rav1g: what is here and what was changed

This directory is a copy of [rav1d](https://github.com/memorysafety/rav1d), the Rust port of dav1d, at
the commit `UPSTREAM` names (`2e7216c`, version 1.1.1 in its `Cargo.toml`, 212 commits behind the tag
`v1.1.0`). rav1g uses its **parse pass**: the code that reads an AV1 stream down to blocks, motion
vectors and coefficients. The pixels are made by rav1g's compute shaders. The design, and the
measurements that led to it, are in `docs/phase2-rav1d.md` of the repository.

Licence: BSD 2-Clause, `COPYING` (VideoLAN, the dav1d authors and the Internet Security Research
Group); `doc/PATENTS` is the patent licence of the Alliance for Open Media that upstream carries;
`THANKS.md` and `README.md` are upstream's. The packages of rav1g that contain this code carry
`COPYING`.

## The rule

Changes only take away, or route what the parse pass reads to the application, or are upstream's own
fixes and optimisations: those of dav1d's later versions (rav1d follows dav1d 1.5.1) that read every
stream the specification allows exactly as before, each with the dav1d commit it follows (the section
"From dav1d" below). Nothing here changes how a syntax element is read; a fix may refuse a stream the
specification does not allow. Two changes are switches of `ParseSettings` that are off unless the application
sets them, and the parse is upstream's without them: the loop filter masks left out, and a tile kept on
its thread (the rows of `decode.rs` and `thread_task.rs` below). rav1g sets both unless it is told not to
(`rav1g_parse::options::ParseOptions`, `-parseopt none` in its tools). An update to a newer upstream is then: copy the files again, apply the list
below again, run the two checks at the end.

**More than one frame in parse** (`ParseSettings::frames`, upstream's frame threads; step 4 of the
design). The parse of a frame is upstream's with any number of them: the frame contexts, the tasks,
the waits of a frame for the CDFs and for the rows of its references. What changed is what stands
between upstream's scheduler and an application that takes the parse and not the pictures, five
things, each in the rows below: the lock of the scheduler is not held while the sink sets a frame up
(`decode.rs`); a frame that is shown again takes no frame context (`obu.rs`); the data that is sent is
parsed to its end without a picture being taken (`parse.rs`); a panic of a worker thread ends the
parse instead of leaving the sender waiting (`thread_task.rs`); the operating point can change
(`internal.rs`). With one frame in parse none of them is on a path that is taken, except the panic.

`upstream_diff.sh <checkout of rav1d at the commit>` holds the directory against upstream: every file
is upstream's byte for byte, or one of the fifteen changed files below, or one of the five files of
rav1g (`PATCHES.md`, `UPSTREAM`, `upstream_diff.sh`, `src/parse.rs`, `src/parse_stats.rs`). It writes the diff of the changed
files (+1098 −5129 lines at this commit) for a review. CI runs it (job `rav1d`).

Size: 67 files, 1.4 MB, 39,300 lines here; upstream has 1,364 files (296 under `src`, 329,000 lines
there).

## Not copied

- Everything outside `src` except `Cargo.toml`, `build.rs`, `rustfmt.toml` and the notices: the tests
  and their data, the tools (`dav1d` command line), the examples, the C headers, CI, meson files.
- `src/arm`, `src/riscv`, `src/loongarch`, `src/ppc` and all of `src/x86` except `msac.asm` and
  `refmvs.asm` (and `src/ext/x86/x86inc.asm`, which both include): the assembly of the pixel code.
  `src/x86/cpuid.asm` is not needed either: the CPU features come from the `raw-cpuid` crate.
- The pixel code: `cdef.rs`, `cdef_apply.rs`, `fg_apply.rs`, `filmgrain.rs`, `ipred.rs`,
  `ipred_prepare.rs`, `itx.rs`, `itx_1d.rs`, `lf_apply.rs`, `loopfilter.rs`, `looprestoration.rs`,
  `lr_apply.rs`, `mc.rs`, `pal.rs`, `wedge.rs`, `pic_or_buf.rs`. Of `filmgrain.rs` the generation of the
  grain templates (`generate_grain_y_rust`, `generate_grain_uv_rust`, 4:2:0) is in the host of rav1g,
  `crates/rav1g/src/host/grain_template.rs`, with this licence in its header: the decoder makes the
  templates on the CPU and applies the grain on the GPU.
- `rust_api.rs`, the safe wrapper around the C API.

## Changed files

| File | Change |
| --- | --- |
| `Cargo.toml` | The package as a library of this workspace: `rlib` only, not published, no profiles and no workspace of its own; the features are `asm`, `bitdepth_8`, `bitdepth_16`. The dependencies are upstream's. |
| `build.rs` | Assembles the two files above for x86-64 and nothing else. The symbols get the prefix `rav1g_dav1d` instead of `dav1d` (`private_prefix` of `config.asm`), so that an application can link dav1d or rav1d next to this copy. |
| `src/msac.rs`, `src/refmvs.rs` | The names of the assembly functions with that prefix. In `refmvs.rs` the candidates of `rav1d_refmvs_find` are sorted by dav1d's bubble sort again (in place, stable, the same order as the standard library's stable sort upstream calls). |
| `src/ctx.rs` | `CaseSet` matches the length of a context update once and calls the closure with that length as a constant (dav1d's `case_set` switches once); upstream matched it again for every array. The closures that write the contexts (`decode.rs`, `recon.rs`) are `#[inline(always)]`, so each array is one store of its size. |
| `src/env.rs` | `BlockContext` without `uvmode`, the chroma mode of the edges: upstream's reconstruction read it, the parse does not (`decode.rs` no longer writes it). |
| `src/cpu.rs` | Without `dav1d_set_cpu_flags_mask` (C API) and without the number of processors (see `lib.rs`). |
| `src/tables.rs` | Without the nine `#[no_mangle] dav1d_*` tables of the pixel assembly. `LO_CTX_BY_RC`: per transform size, the lo context offset and the hi context of each position of the 2D scan, built at compile time from the same 5x5 offsets (`LO_CTX_OFFSETS_5X5`, which `DAV1D_LO_CTX_OFFSETS` is now made of). |
| `src/lib.rs` | Without the C API (the 19 `dav1d_*` functions) and the modules above; no film grain. `rav1d_open` takes the sink and the two switches of `ParseSettings`. The number of threads and of frames in parse is what the application says (0 is 1): upstream takes the number of processors and its square root, which on Windows are two imports the library does not have otherwise. A crate-level `allow` for the code the removals leave unused, so that the files stay close to upstream. `pub mod parse`, `pub mod levels`. |
| `src/internal.rs` | Without the pixel DSP tables, the film grain task data and the buffers of the reconstruction pass (blocks, coefficients, palette indexes and colours per frame; CDEF and loop restoration line buffers). The context has the sink and the parse switches; a frame has its number at the sink. The operating point of the context is a number an application may change (`Parser::set_operating_point`); it is read where upstream reads it, at a sequence header. The context has a mark for "a worker thread ended with a panic" (`broken`, see `thread_task.rs`). |
| `src/obu.rs` | The sink sees a sequence header before the parser takes it and can refuse it, and it is told when a frame header shows the frame of a reference slot (`show_existing_frame`), before the slots are updated. Nothing of the reading changes. With more than one frame in parse upstream appends the picture of such a header to its queue of pictures: it takes the next frame context and waits for the frame that had it, so a stream that shows a hidden frame in every other temporal unit never has two frames in parse. Here the sink has been told, it keeps the order of the output itself, and the pictures are not used: a shown frame takes no frame context and waits for nothing. The sink is given two things upstream reads and an application wants as they are: with a sequence header the three code points of its colour description as they are coded (the header keeps them as enums, in which a value the specification reserves is "unspecified"), and with a shown frame the HDR metadata the parser has at that header (content light level, mastering display), which is what upstream attaches to the picture it outputs. |
| `src/decode.rs` | The parse pass only. A block is handed to the sink when its mode info is read, and its coefficients are read right behind it (upstream stores the block and reads the coefficients in the same pass; the second pass reconstructs). The palette index map goes to the sink as it is read (upstream packs it for the second pass). The sink is told when a frame, a tile and a superblock begin and end, and a frame comes with the HDR metadata upstream attaches to its picture; the loop restoration units of a superblock are collected for it. No reconstruction, no bookkeeping of the lowest pixel a block predicts from, no pixel buffers. The reference handling, the CDF updates and the motion vector storage are upstream's. The sink says at the end of a frame whether the frame stands: a frame it could not take fails like a frame with damaged data (its reference slots are emptied, as upstream empties them). The parse of a superblock row of a tile is timed for the sink, when the sink has a clock. With `ParseSettings::lf_masks` off, the two calls that fill the masks and levels of upstream's loop filter for a block (`rav1d_create_lf_mask_intra`, `rav1d_create_lf_mask_inter`) are left out: the parse reads neither, and they are a quarter of the parse of a 4K frame with few coefficients. With more than one frame in parse, `rav1d_submit_frame` gives the lock of the scheduler back as soon as it has the frame context, and takes it again to queue the frame's first task. Upstream holds it to the end of the function, across the allocation of the picture; here the function calls the sink, which sets the frame up and may wait for room that the frames in parse give back, and their threads need that lock to go on (a wait for ever otherwise, and without the wait the parse of the other frames would stand still while a frame is set up). Nothing in between needs the lock: the frame context has no task and no thread works on it. The two places that wait for a frame (the frame context in `rav1d_submit_frame`, the tiles in `rav1d_decode_frame`) stop waiting in a context that is broken (`thread_task.rs`) and return an error. A frame tells the sink whether its tiles are parsed by the worker threads (`ParseFrame::tile_tasks`, more than one thread). The number the sink returns at a tile's begin is kept in the tile's state (`Rav1dTileState::sink_tile`, `internal.rs`) and comes with the tile's block, transform block, palette and superblock calls; a block's call has no palette colours (they come with its palette calls). Three writes of upstream's that only its reconstruction read are left out: the chroma mode of the edges (`uvmode`), the luma mode of an inter block in the mode context (read for `y_mode` of key and intra-only frames only, which have no inter blocks), and, unless `ParseSettings::lf_masks` is on, the mask of the blocks with coefficients for its CDEF (`noskip_mask`). |
| `src/recon.rs` | Reading of the coefficients and of the palette colours only. `decode_coefs` asks the sink where the coefficients of a transform block go and writes them there dequantised, 32 bits each, in scan order. Each nonzero AC token packs its magnitude and scan index in the existing task scratch, so the sign pass reads it sequentially and writes directly to the sink. The values and contexts are computed as upstream computes them. `get_skip_ctx` and its helpers are always inlined; the checks that a dequantised coefficient stays below `cf_max` (or 24 bits with a quantiser matrix) are debug asserts, as in dav1d. In the loop of the 2D tokens the context offset and the hi context come from one entry of `LO_CTX_BY_RC` at the position, not from `x` and `y` (the values are the same). |
| `src/thread_task.rs` | Without the film grain tasks; the filter tasks do nothing. The reconstruction task of a tile returns at once. Entropy row readiness follows the motion fields selected for projection and the primary reference's previous segmentation map when present. Named reference slots are still checked for errors, and the CDF and progress signals are as upstream uses them. The same timer around the parse of a superblock row as in `decode.rs`. With `ParseSettings::tile_keeps_thread`, a tile whose next superblock row waits for nothing goes on with it on its thread. Upstream has that path and never takes it: it asks `check_tile` before it stores the progress of the row that has just ended, so the answer is "not yet", the tile goes back to the queue after every row, a waiting thread is woken, and the next row runs on whichever thread takes it. With the switch the progress is stored first; with one frame in parse no thread is woken for the row either (the filter tasks of a row do nothing here and run when the tile is through), with more than one the signal is upstream's. A panic of a worker thread is caught (`rav1d_worker_task` around upstream's function, now `worker_task`): upstream lets the thread end, and the thread that waits for the frame it worked on waits for ever. Here the context is marked broken, the waiting thread is woken and returns an error, the other workers take no more tasks, and the thread counts as parked for `rav1d_flush`, so that the context can be closed. A panic is a bug, of the parser or of the sink; a damaged stream is an error as before. **Work of the sink for an idle worker** (rav1g's block lists a superblock row behind the entropy decoder): a worker that finds no task asks the sink for work (`ParseSink::idle_work_pending`, `idle_work`) before it waits, and says first that it waits (`cond_signaled`), so that the thread of a tile that has parsed a row and finds work at the sink wakes it under the lock it waits with; the thread of a tile's last row, behind the save of the frame's CDFs and the wake-up of a thread for what waited for them, tells the sink that the tile is through (`ParseSink::tile_finish`) without the lock, in the pass that parses (not the reconstruction pass, which parses nothing), before the frame's task count goes down. A sink without that work (the defaults of the three methods) leaves the scheduling upstream's. |
| `src/picture.rs` | A picture has no samples: each plane is the smallest piece of memory a plane can be. Pictures remain the handles of the reference bookkeeping. |

## From dav1d

Fixes and optimisations of dav1d after 1.5.1 (the version rav1d's assembly and logic follow), ported to
the Rust of this copy. They are listed here and not in the rows of the files above, which are rav1g's own
changes.

| dav1d commit | File | What |
| --- | --- | --- |
| `2a0775ca` (master) | `src/thread_task.rs` | `rav1d_task_frame_init` clears the scan position of the frame context's task queue (`tasks.cur_prev`). A frame aborted in its `InitCdf` task (its CDFs come from a frame that failed) after a scan passed over that task keeps one; when its frame context is taken again, `reset_task_cur` takes a frame after `cur` to have none, and the scans of the new frame start behind its tasks: every worker asleep, the sender waiting. Only after a decoding error, with more than one frame in parse. |

## Added

- `src/parse_stats.rs`: the times of each frame of the frame-parallel parse, for `rav1g-parse-check -time
  -frametimes`: its wait for a frame context in `rav1d_submit_frame`, its `Init` and `InitCdf` tasks and
  the wait between them for the CDFs it loads, its entropy tile tasks, and the time without a tile running
  after `check_tile` held a tile back for the rows of a reference. The hooks are in `decode.rs`
  (`rav1d_submit_frame`, `rav1d_decode_frame_exit`), `thread_task.rs` (the `Init`, `InitCdf` and tile
  tasks, `check_tile`), a field of `Rav1dFrameContextTaskThread` (`internal.rs`) and `pub mod parse_stats`
  (`lib.rs`). They are off unless the tool switches them on (one relaxed load of a flag each), and they
  decide nothing.
- `src/parse.rs`: the interface to the application. `ParseSink` is called per sequence header, frame,
  shown reference slot, tile, block, coded transform block, palette plane and superblock; `Parser` takes
  the temporal units of a stream; `ParseSettings` has its threads, the frames in parse and the two
  switches above. With more than one frame in parse `Parser::send` does not go through upstream's
  queue of pictures (`rav1d_send_data`, `rav1d_get_picture`), which is made for an application that
  takes them in the order of the stream: a picture that is ready stops the parse of the data until it
  is taken, the second call for a picture in a row waits for every frame in parse, and the error of
  an earlier frame comes back in place of a picture, with which the rest of the data would be
  dropped. It calls upstream's `gen_picture` (now visible in the crate) until the data is parsed and
  empties the queue in between; how a frame's parse ended is told to the sink (`frame_end`) and to
  nobody else. `Parser::wait` waits until no frame is in parse and keeps the state of the stream;
  `Parser::set_operating_point`, `Parser::frames` and `Parser::broken` are what their names say. A frame and a shown slot come with the place of their OBU in the data
  that was sent (`data_left`, which `lib.rs` and `internal.rs` keep per OBU), for an application that
  decides itself which frame of a temporal unit is output. `ParseColorCodes` (with a sequence header)
  and `ParseHdrMetadata` (with a frame and a shown slot) are what the stream says about its colour, for
  the application to pass on: the parse does not use them. After an error `Parser::send` leaves nothing
  of the data with the parser: upstream keeps unread data when a picture is ready, which would be read
  again in front of the next data.
- `PATCHES.md`, `UPSTREAM`, `upstream_diff.sh`.

## Checks

- `upstream_diff.sh`, above.
- Upstream itself at this commit, unchanged, decodes the conformance streams of rav1g to their
  reference md5 on Linux (CI job `rav1d`): the base the changes start from is right.
- The changed parser against the entropy decoder of libaom on the same streams, every block, transform
  block and coefficient (`tools/rav1g-parse-check`, CI job `parse-check`).
