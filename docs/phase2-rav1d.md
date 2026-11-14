# Phase 2: rav1d's parser in place of the C parser: design

Status: design, written 2026-11-12, with measurements on a development machine; steps 1 to 3 of the
[plan](#plan) are in the tree ([Progress](#progress)): the decoder runs on either parser, chosen when it is
created, and since the console timed them rav1d's is the default
([Console after step 3](#console-after-step-3-rav1d-is-the-default)). Phase 1 ([rust-port.md](rust-port.md)) moved the host to Rust and kept the libaom-derived C parser;
phase 2 replaces that parser with the one of rav1d (<https://github.com/memorysafety/rav1d>, the Rust port
of dav1d, BSD-2-Clause).

## Summary

- **The cut is rav1d's first pass.** With frame threads rav1d (like dav1d) decodes every tile row twice:
  a parse pass that stores blocks, coefficients and palette data, and a reconstruction pass. The parse pass
  needs nothing back from reconstruction: no pixel is read. rav1g keeps the parse pass with everything it
  depends on (OBU and header parsing, CDFs and their adaptation, motion vector prediction, segmentation
  maps, palette and coefficient reading, reference bookkeeping, the task scheduler) and drops the rest.
- **The hand-off keeps today's GPU formats.** The parse pass calls a sink in the host for every block,
  as the C parser calls `av1_mi_push_block` today. The sink writes the mode info record, the grid and the
  block lists the shaders read now; the coefficient reader writes dequantised coefficients in scan order
  straight into the mapped coefficient buffer. No shader changes, and the C parser is a byte-exact oracle
  for the new code. rav1d has to be vendored and patched for this: its public API returns pictures only.
- **Measured: rav1d's parse pass takes 0.42 to 0.44 of the time of the current parser on heavy 4K 10-bit
  at 40 Mbps** (22.0 against 51.8 ms of CPU time per frame, and 19.3 against 44.1, on the same machine),
  0.64 to 0.70 on lighter 4K streams. Scaled to the console, where the current parser takes 44 and 34 ms
  on these two clips: about 19 and 15 ms per frame on one core.
- **So one parse thread is enough for 4K30 on the CPU side**, 95th percentile included (about 27 ms).
  The faster parser is the gain, not frame parallelism: a frame's parse needs the final CDFs of an earlier
  frame, and on the 1-tile 4K clips that chain limits two frame threads to 1.4 to 1.8 times one, three to
  at most 2.2.
- **After phase 2 the GPU is not the limit either.** Measured on the console with `-gputime` (2026-11-12,
  packages 0.2.116.2/.3 and 0.2.117.0, two runs each): the GPU is busy 18.5 ms per frame on the heaviest
  clip (`j4k40`, p50 15.1 ms) and 12.1 ms on the SVT low-delay 40 Mbps clip, while the decode thread takes
  50.3 and 43.5 ms, 42.6 and 34.7 of them in the parser. The figures of about 50 and 31 ms GPU time used
  earlier came from `-gputimedetail`, whose per-stage timestamps and barriers inflate GPU time (same clip,
  same shaders: 49.3 ms with `-gputimedetail`, 18.5 ms without). With the parser at about 20 ms, both
  clips are projected under 33.3 ms.
- **Memory:** with one frame parsed at a time the D3D12 buffers stay as they are. A second frame in parse
  costs one more slot (about 110 MiB at 4K 10-bit) until the shares of a slot are replaced by a shared
  pool; the measured frames fill 1 to 11 % of a share on average, the largest one 44 %.
- **Efficiency:** about 0.6 of a core for 4K30 at 40 Mbps, where the current parser would need 1.3 cores
  of work that cannot be split on a 1-tile stream.

## Goal and scope

The goals of the project, in order: heavy 4K at 30 fps in real time (at most 33.3 ms per frame), then
faster, then efficiency (CPU, GPU and memory left to the application). The target is the console: a UWP
application, D3D12, eight Zen 2 cores of which the application gets some, 1 GiB of application memory.

Today the CPU limits heavy 4K: the parser takes 34 to 45 ms per frame on one core on 1-tile streams, and
setup and recording share its thread ([gpu-av1-decoder.md](gpu-av1-decoder.md), "Next CPU target").

Phase 2 changes the parser and the boundary between parser and host. It does not change the shaders, the
stages of the GPU pipeline or the interfaces of the library (`rav1g::Decoder`, `rav1g.h`). Output stays
byte-identical.

## What was measured

Machine: Intel Core i7-10610U (4 cores, 8 threads, AVX2, 15 W), Linux 7.2, 16 GB. It is a laptop: short
runs get a higher clock than long ones, so each table says which it is. rav1d: commit
`2e7216c20ac2a75de204d6c8d7268b75a45959b4` (version 1.1.1, 2026-10-27), `cargo +stable build --release`
(rustc 1.99.1, its release profile: fat LTO, one codegen unit, panic abort), assembly on (nasm 3.02).
Film grain off in every run, output discarded.

Clips, 300 frames each (local, not in the repository):

| Name | What |
| --- | --- |
| `j4k40` | 4K 10-bit, 40 Mbps, 1 tile, every frame shown; the heaviest console clip |
| `t40x1ld` | 4K 10-bit, SVT-AV1 low delay, 40 Mbps, 1 tile |
| `t12x1` | 4K 10-bit, SVT-AV1 random access, 12.6 Mbps, 1 tile, hidden frames |
| `fg4k_300` | 4K 10-bit, libaom, film grain, 8 tile columns, hidden frames |
| `a1_8b4k_300` | 4K 8-bit, 1 tile, hidden frames |

### rav1d as it is: whole decode

Short runs (5 to 13 s). Threads 2 and 4 give two frame contexts, 8 gives three (rav1d's default).

| Clip | 1 thread | 2 | 4 | 8 | CPU seconds, 1 → 8 threads |
| --- | ---: | ---: | ---: | ---: | --- |
| `j4k40` | 23.9 fps | 38.6 | 52.7 | 59.6 | 12.3 → 25.8 |
| `t40x1ld` | 32.1 | 51.5 | 62.7 | 69.0 | 9.3 → 16.8 |
| `fg4k_300` | 27.2 | 44.3 | 61.0 | 64.5 | 10.9 → 29.3 |
| `a1_8b4k_300` | 63.8 | 99.6 | 107.8 | 99.6 | 4.6 → 9.8 |

This is software decoding and only here for scale: the machine has four cores, and the largest resident
size grows from 212 MB (one thread) to 483 MB (eight) on `j4k40`.

### The parse pass alone

A throwaway patch (see [The experiment patch](#the-experiment-patch)) times the `TileEntropy` tasks and,
with a switch, makes the reconstruction pass and the filters return at once, so that only the parse pass
and the scheduler run. Frame contexts equal to worker threads; short runs.

| Clip | Parse, CPU ms per frame | 2 threads | 3 | 4 | 6 | 8 |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| `j4k40` | 18.3 | 91.9 fps | 100.4 | 126.8 | 120.1 | 119.9 |
| `t40x1ld` | 15.4 | 92.5 | 98.5 | 113.7 | 117.1 | 117.0 |
| `t12x1` | 6.8 | 187.0 | 213.6 | 214.7 | 249.3 | 240.5 |
| `fg4k_300` | 11.8 | 141.2 | 181.7 | 191.9 | 218.4 | 225.0 |
| `a1_8b4k_300` | 6.5 | 182.3 | 204.4 | 209.6 | 210.1 | 211.3 |

- The parse pass is 44 % of rav1d's single-threaded decode of `j4k40` (18.3 of 41.8 ms per frame).
- Two threads parse two frames at once where the stream allows it: 1.7 times the serial rate on `j4k40`
  (one frame per 18.3 ms would be 54.6 fps), 1.4 times on `t40x1ld`.
- More than four threads are hyper-threads here: the CPU time per frame rises (to 25 to 27 ms on `j4k40`)
  and the rate does not. The console's cores are real ones; the next section says what the streams allow.

### What the streams allow: the CDF chain

A frame starts from the CDFs an earlier frame ended with (`primary_ref_frame`; all clips update the CDFs
at the end of the frame, none sets `disable_cdf_update`). The patch logs for each frame how many frames
back, in decode order, that earlier frame lies:

| Clip | 1 back | 2 | 3 | 4 | further | none (default CDFs) | temporal motion vectors |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | --- |
| `j4k40` | 25 % | 0 | 25 % | 49 % | 0 | 1 frame | off |
| `t40x1ld` | 50 % | 25 % | 0 | 25 % | 0 | 1 frame | off |
| `t12x1` | 26 % | 25 % | 12 % | 6 % | 31 % | 1 frame | on 9 % of the frames |
| `a1_8b4k_300` | 16 % | 2 % | 24 % | 18 % | 39 % | 1 % | off |
| `fg4k_300` | 63 % | 7 % | 14 % | 0 | 10 % | 6 % | on every inter frame |

A frame whose source is the frame right before it cannot overlap with it at all on a 1-tile stream. A
schedule computed from these dependencies alone (a frame starts when its CDF source has finished, frames
start in decode order, cost of a frame proportional to its bytes) gives, in multiples of one thread:

| Clip | 2 threads | 3 | 4 | 6 |
| --- | ---: | ---: | ---: | ---: |
| `j4k40` | 1.76 | 2.16 | 3.10 | 3.10 |
| `t40x1ld` | 1.53 | 1.53 | 1.53 | 1.53 |
| `t12x1` | 1.78 | 2.12 | 2.79 | 3.09 |
| `a1_8b4k_300` | 1.44 | 1.69 | 1.69 | 1.75 |

The measured 1.7 and 1.4 with two threads agree with it. rav1d also makes the parse of a frame wait, tile
row by tile row, for the parse of all seven references, which is only needed for temporal motion vectors
and for the segmentation map of the primary reference. Waiting only where needed changed nothing
(`j4k40` 92.5 against 92.5 fps with two threads, 126.8 against 129.6 with four): the CDF chain is the limit.

### Against the current parser, same machine

The current parser was timed with `rav1g-dec -hw -gputime -t 1` under wine (vkd3d on lavapipe, two
lavapipe threads): `entropy_sum`, the time of the tile decode with the block lists, of the clang-cl build
with link time optimisation of the C; the MSVC build of CI gave the same on `j4k40` (51.9 ms). The run
takes minutes, so the clock was the sustained one (2.7 to 3.1 GHz sampled). rav1d's parse pass was
therefore timed again in the same state: the 300 frames in a loop for a minute or more, two frame threads
and a busy thread next to them (2.6 to 2.8 GHz sampled, so slightly to rav1d's disadvantage). Milliseconds
per frame, without the first ten frames:

| Clip | Current parser, mean (p50, p95) | rav1d parse pass, mean (p50, p95) | Ratio of the means |
| --- | ---: | ---: | ---: |
| `j4k40` | 51.8 (50.6, 81.5) | 22.0 (20.8, 31) | 0.42 |
| `t40x1ld` | 44.1 (37.7, 83.8) | 19.3 (18.2, 34) | 0.44 |
| `fg4k_300` | 21.4 (10.7, 84.7) | 13.7 (11.6, 38) | 0.64 |
| `a1_8b4k_300` | 10.0 (3.9, 68.0) | 7.0 (3.2, 38) | 0.70 |

- rav1d's parse pass is 2.3 to 2.4 times as fast where coefficients dominate, 1.4 to 1.6 times on the
  lighter streams, and its large frames are much cheaper (p95 31 against 81.5 ms).
- What each side contains is not quite the same. The current parser's time includes its block lists, the
  mode info grid and the dequantised coefficients in the GPU's layout. rav1d's includes things rav1g will
  not need (loop filter masks, the lowest reference row of every inter block, a dense coefficient buffer of
  12.7 MB per frame on `j4k40`) and lacks the sink of this design (about 1 ms on a heavy frame, see
  [CPU cost](#cpu-cost-of-the-hand-off)).

### Data per frame

Counted by the patch in the parse pass; maxima from single-threaded runs.

| Clip | Blocks, mean (max) | Coded transform blocks, mean | Coefficients up to the last one, rounded up to 4, mean (max) | The same as rav1d stores them (whole transform blocks), mean |
| --- | ---: | ---: | ---: | ---: |
| `j4k40` | 12,958 (27,584) | 14,311 | 466,398 (660,380) | 3,195,156 |
| `t40x1ld` | 3,438 (15,582) | 5,441 | 692,409 (1,276,664) | 3,703,797 |
| `t12x1` | 2,888 (18,648) | 3,338 | 236,595 (1,018,608) | 2,374,784 |
| `fg4k_300` | 3,817 (12,095) | 3,428 | 125,703 (2,755,292) | 999,354 |
| `a1_8b4k_300` | 3,754 (27,678) | 2,605 | 78,820 (2,247,532) | 315,562 |

Two things follow. The layout of today's coefficient buffer (scan order up to the last coefficient) is 4
to 10 times smaller than rav1d's own, so the hand-off writes that and not rav1d's. And a 4K frame fills
little of what a slot reserves for it: at most 27,678 mode info records (6.2 MB, a low-memory share is
27.9 MiB) and 2.76 million coefficients (11 MB, 44 % of a share of 23.9 MiB); the means are 0.6 to 2.9 MB
and 0.3 to 2.8 MB, 1 to 11 % of the shares.

## Where rav1d is cut

### The two passes, in the source

- `src/thread_task.rs`, `rav1d_worker_task`: with more than one frame context every tile row is two tasks.
  `TileEntropy` runs `rav1d_decode_tile_sbrow` with `tc.frame_thread.pass = 1`, `TileReconstruction` with
  `2`; with one frame context there is one task and `pass = 0` does both at once. The filters are tasks
  of their own behind them (`DeblockCols`, `DeblockRows`, `Cdef`, `SuperResolution`, `LoopRestoration`),
  film grain is `FgPrep` and `FgApply`.
- `src/decode.rs`, `decode_b`: the first pass (`FrameThreadPassState::First`) reads the block from the
  bitstream into `f.frame_thread.b[t.b.y * f.b4_stride + t.b.x]` and, where pass 0 calls `recon_b_intra`
  or `recon_b_inter`, calls `read_coef_blocks` instead. The second pass takes the stored block, calls
  `recon_b_*`, repeats a few context updates and returns.
- `src/recon.rs`, `rav1d_read_coef_blocks` and `read_coef_tree`: every transform block goes through
  `decode_coefs`, which writes into `f.frame_thread.cf` (`CfSelect::Frame`), and its last coefficient and
  transform type go to `f.frame_thread.cbi`. Palette colours go to `f.frame_thread.pal`, the index map,
  packed, to `f.frame_thread.pal_idx`.
- The first pass also produces, per tile row: the loop filter masks and levels (`rav1d_create_lf_mask_*`,
  `f.lf.mask`, `f.lf.level`), the CDEF index of each 64x64 (`f.lf.mask[..].cdef_idx`), the loop
  restoration units (`read_restoration_info`, `f.lf.lr_mask`), the segmentation map (`f.cur_segmap`), the
  motion vectors for later frames (`save_tmvs` into the picture's `mvs`), the lowest reference row each
  block reads (`f.lowest_pixel_mem`, for the waits of the second pass), and at the end of the tile named by
  `context_update_tile_id` the CDFs for later frames (`rav1d_cdf_thread_update` into `f.out_cdf`).

### What the parse pass waits for

- `InitCdf` waits for `fc.in_cdf().progress()`: the frame whose CDFs this one starts from must have
  finished the parse of its context tile.
- `check_tile`, for a `TileEntropy` task: each of the seven references must have parsed the same
  superblock row (its entropy progress, `progress[0]`). That serves temporal motion vectors (`load_tmvs` reads the references'
  `mvs`) and the previous segmentation map.
- Only `TileReconstruction` waits for pixels (`progress[1]`, up to the lowest row read).

### What the parser needs back from reconstruction

Nothing. No symbol's meaning depends on a reconstructed sample, and the first pass reads none. From
earlier frames it reads: the CDFs (`state.cdf[..]`), the segmentation map and the motion vectors of
reference pictures (`state.refs[..].segmap`, `.refmvs`), and header state that the references carry (sizes,
order hints, global motion, loop filter deltas, film grain parameters). All of it is made by parsing.

From the host it needs two things:

- **A picture for every frame.** `rav1d_submit_frame` refuses a frame whose reference has no picture data
  (`state.refs[..].p.p.data.is_none()`). The host supplies the picture allocator; a picture is a handle of
  the host with no pixels behind it. rav1d's reference counting then tells the host when no later frame
  can name the picture any more (the release callback).
- **Room.** The sink may block the thread that submits a frame until a slot is free; that is the back
  pressure the decode thread has today in `av1_setup_frame`.

### Kept and dropped

Kept (about 33,000 of rav1d's 55,000 lines of Rust, 1,700 lines of assembly): `obu.rs`, `decode.rs`, the coefficient reader of
`recon.rs` (the first 1,760 lines), `msac.rs`, `cdf.rs`, `env.rs`, `refmvs.rs`, `warpmv.rs`, `pal.rs`,
`intra_edge.rs`, the tables, `picture.rs`, `thread_task.rs`, `lib.rs`, `internal.rs`; `msac.asm`,
`refmvs.asm`. (`cpuid.asm` was on this list; rav1d does not use it, the CPU features come from the
`raw-cpuid` crate. `pal.rs` went with the pixel code: its one function packs the palette indexes for the
second pass. [Progress](#progress) has what the copy is.)

Dropped: the second pass (`recon_b_intra`, `recon_b_inter` and below), `ipred`, `mc`, `itx`,
`loopfilter`, `cdef`, `looprestoration`, `filmgrain` and their `*_apply` files, 44 of the 47 x86 assembly
files, the pixel buffers (`ipred_edge`, the CDEF and loop restoration line buffers, the pictures), and
`f.frame_thread.b`, `cbi`, `cf`, `pal`, `pal_idx`, which the sink replaces. The loop filter masks and
the lowest-row bookkeeping can stay in the first step and go later; the GPU builds its edge lists from
the mode info.

### How it is run

A mode of the patched rav1d ("parse only"): every frame goes through the task scheduler with pass 1 and
nothing else, whatever the number of frame contexts. `Init`, `InitCdf`, one `TileEntropy` chain per tile,
then the frame is done. Frame contexts are the frames that may be in parse at once; worker threads serve
tiles and frames alike.

rav1d must be vendored: its Rust API (`src/rust_api.rs`: `Decoder`, `Settings`, `Picture`) and its C API
hand out pictures and nothing of the parse, and everything named above is `pub(crate)`. Proposed:
`third_party/rav1d` at the pinned commit with rav1g's changes as a short list of patches on top, so that a
newer rav1d is a rebase.

## The hand-off

### The sink

The parse pass calls the host at fixed points. In Rust terms (names are placeholders):

| Call | Where in rav1d, which thread | What the host does |
| --- | --- | --- |
| `sequence` | a new sequence header, submitting thread | fits or reconfigures the decoder (`av1_sequence_fits_decoder` today) |
| `frame_begin` | `rav1d_submit_frame`, submitting thread, decode order | takes a slot (may wait), sets up the frame's constants and tile data; holds the frame's references |
| `block` | end of the first pass of `decode_b`, worker thread | writes the mode info record and the grid, then counts the block into the lists (`av1_mi_push_block`) |
| `transform block` | `read_coef_blocks`, `read_coef_tree`, worker thread | the 16-byte record of the block; the coefficients are already in the mapped buffer |
| `palette` | after `read_pal_indices`, worker thread | writes the block's pixels into the palette buffer (`av1_intra_palette`) |
| `superblock` | `rav1d_decode_tile_sbrow`, worker thread | CDEF index of each 64x64, loop restoration units |
| `frame_end` | the last tile's last row, worker thread | queues the frame for recording and submission in decode order |
| `show_existing` | the `show_existing_frame` branch of `obu.rs`, submitting thread | queues an output copy of the picture (`av1_decode_sef`) |
| picture alloc, release | rav1d's picture allocator | hands out and takes back picture handles |

It is a trait the host implements and the parser is generic over, or a table of functions; one call per
block is not what costs (a block of `j4k40` is 1.7 microseconds of parsing on average).

### Blocks

The shaders read the mode info pool as 224-byte records (`libav1/dx/shaders/mode_info.h`, the layout of
`MB_MODE_INFO` in `libav1/av1/common/blockd.h`), found through a grid with one 8-byte pointer per 4x4
unit. Three shaders read records (`gen_pred_blocks.hlsl`, `gen_lf_blocks_vert.hlsl`,
`gen_lf_blocks_hor.hlsl`); the host reads them in `av1_mi_push_block` (`crates/rav1g/src/host/blocks.rs`),
in the CDEF and loop restoration lists and for palette blocks. rav1d's block is `Av1Block`
(`src/levels.rs`): `bl`, `bs`, `bp`, `seg_id`, `skip_mode`, `skip`, `uvtx` and either `Av1BlockIntra` or
`Av1BlockInter`.

| Record field | Read by | From rav1d | Kind |
| --- | --- | --- | --- |
| `sb_type` | shaders, host | `bs` (`BlockSize`, largest first) | table of 22 |
| `partition` | host (top-right and bottom-left availability) | `bp` (`BlockPartition`, same order) | copy |
| `mi_row`, `mi_col` | shaders, host | `t.b.y`, `t.b.x` | copy |
| `ref_frame[2]` | shaders, host | `ref[2]` plus one; intra 0 and none; inter-intra: second is 0 | cheap |
| `mode` | shaders, host | `y_mode` (intra: same numbers), `inter_mode` plus 13, or plus 17 for compound | cheap |
| `uv_mode` | `gen_pred_blocks` | `uv_mode` (CFL is 13 in both) | copy |
| `angle_delta[2]` | `gen_pred_blocks` | `y_angle`, `uv_angle` | copy |
| `filter_intra_mode_info` | `gen_pred_blocks` | `y_mode == FILTER_PRED`, the filter in `y_angle` | cheap |
| `cfl_alpha_idx`, `cfl_alpha_signs` | `gen_pred_blocks` | `cfl_alpha[2]`, the two signed values | cheap (index and joint sign back from the values) |
| `palette_size[2]` | `gen_pred_blocks` | `pal_sz[2]` | copy |
| palette colours (48 bytes) | host only, for the pixels | rav1d's palette of the block | not in the record; see `palette` above |
| `tx_size` | shaders, host | intra `tx`, inter `max_ytx`; 4x4 when lossless | copy |
| `inter_tx_size[16]` | loop filter lists (inter blocks with coefficients) | `max_ytx`, `tx_split0`, `tx_split1` | cheap: the split tree written out, at most 16 entries |
| `skip` | loop filter lists, CDEF list | `skip` | copy |
| `segment_id` | loop filter lists, host | `seg_id` | copy |
| `mv[2]` | `gen_pred_blocks` | `nd.one_d.mv[2]` (`y`, `x` in eighths, the layout of `MV`) | copy |
| `interp_filters` | `gen_pred_blocks` | `filter2d` | table of 10 |
| `motion_mode` | shaders, host | `motion_mode` (same order) | copy |
| `wm_params` (matrix, shears, invalid) | `gen_pred_blocks`, host | `t.warpmv` of the task at the time of the block | copy of the values the specification defines |
| `interinter_comp` (type, mask type, wedge index, sign) | `gen_pred_blocks`, host | `comp_type`, `mask_sign`, `wedge_idx` | table of 4 |
| `interintra_mode`, `use_wedge_interintra`, `interintra_wedge_index` | `gen_pred_blocks`, host | `interintra_mode`, `interintra_type`, `wedge_idx` | cheap |
| `use_intrabc` | shaders, host | an inter block in a frame with `allow_intrabc` | cheap |
| `delta_lf_from_base`, `delta_lf[4]` | loop filter lists | `ts.last_delta_lf` | copy |
| `index_base` | `gen_pred_blocks` | the host's own | as today |
| `cdef_strength` | host, CDEF list | `f.lf.mask[..].cdef_idx`, per 64x64 | straight into `cdef_indexes`, no record field |
| `current_qindex`, `num_proj_ref`, `comp_group_idx`, `compound_idx`, `skip_mode`, `seg_id_predicted`, `ref_mv_idx`, `interintra_wedge_sign` | no shader | not needed (the sign is 0) | dropped or constant |

No field needs a shader change. What `av1_mi_push_block` takes from the C parser besides the record
(`up_available`, `left_available`, `mi_stride`, `cur_frame_force_integer_mv`, `lossless[]`, the tile's
data) the sink has from the tile and the frame header. Its two calls into libaom, `has_top_right` and
`has_bottom_left` of `reconintra.c`, are ported with their tables (pure functions of block size,
partition and position); neighbours are read from the host's own grid, as now.

### Coefficients

| GPU input | Written today by | In rav1d | Kind |
| --- | --- | --- | --- |
| `idct_coefs`: 32-bit dequantised coefficients in scan order, the count rounded up to four, zero elsewhere (the transform shader clears what it reads) | `av1_read_coeffs_txb` (`libav1/av1/decoder/decodetxb.c`) | `decode_coefs` writes each coefficient at its position in the transform block, 16-bit for 8-bit video, 32-bit above | a change in `decode_coefs`: the second loop, which visits the non-zero coefficients in rising scan order to read sign and remainder and to dequantise, stores at `base + inverse_scan[position]` in the mapped buffer; the token loop keeps the task's own block as scratch |
| `idct_blocks_unordered`: 16 bytes per coded transform block (count, type bits, plane, scan class; coefficient offset; position; sort key) | the same function | last coefficient and `TxfmType` from `decode_coefs`, size and position from the loops of `read_coef_blocks` | cheap; `TxfmSize` and `TxfmType` have the numbers of `TX_SIZE` and `TX_TYPE` |
| the tile's counts per transform size | the same function | the sink, per tile | as today |

- Dequantisation is the same arithmetic in both (`level * dqv`, masked to 24 bits, shifted; the quantiser
  matrix as `(dqv * qm + 16) >> 5`). rav1d also clamps to the range of the bit depth; the transform
  shader clamps its input to that range (`row_clamp`), so the samples are the same.
- The writes stay as sequential as today's: rising scan order within a block, blocks in decode order.
- Writing rav1d's own layout instead would move 4 to 10 times the bytes ([Data per frame](#data-per-frame))
  and need the 19 transform shaders to read another layout. Not proposed.

### Frame level

The host reads about 60 fields of the frame and sequence headers through `AV1_COMMON` (frame and tile
sizes, loop filter levels and deltas, segmentation features, CDEF and loop restoration parameters, the
superres denominator, global motion, reference order and scale factors, film grain; mostly in
`frame.rs`, `loopfilter.rs`, `postfilter.rs`, `prediction.rs`, `film_grain.rs`). `Rav1dFrameHeader` and
`Rav1dSequenceHeader` hold the same syntax elements; a small module of the host turns them into what the
stage modules take, so those do not see which parser ran. Loop restoration units come from
`f.lf.lr_mask` instead of `cm->rst_info`. The quantiser tables are no longer the host's business.

Which frame of a temporal unit is output (`av1_frame_is_output` of `obu.c`, a look ahead over OBU headers)
moves to Rust with the rest of what `av1_dx_iface.c` does around the parser.

### CPU cost of the hand-off

- The record and the grid: a microbenchmark of a translation of this shape (the fields above into a
  224-byte record, the pointer written to every 4x4 unit of the block) takes 52 to 96 ns per block on the
  development machine: 0.33 ms per frame at 3,438 blocks, 0.85 ms at 12,958 (the mean of `j4k40`),
  1.45 ms at 27,678 (the largest frame seen). Most of it is the grid, 4.2 MB per 4K frame, which the C
  parser fills as well.
- The block lists are the code of today: 131 to 143 microseconds per frame for the 3,754 blocks of the
  light 4K stream on a CI runner, so about 0.5 ms for the 13,000 of `j4k40`.
- Transform block records: 22,000 per frame on `j4k40`, a few nanoseconds each.

Together about 1 to 1.5 ms on a heavy 4K frame, 0.5 ms on a light one, on the parse thread. Against
that, the first pass no longer fills `f.frame_thread.b`, `cbi` and `cf`.

## Frame parallelism and the host

### What the host has to change

Today one frame is being built at a time: `Av1Core::curr_frame_data`, the timers of "the" frame, the
functions the tile threads call without saying which frame. With rav1d the parse of a frame is a set of
tasks on worker threads, and with more than one frame context several frames are in parse.

1. **A frame is a value, not the current one.** `frame_begin` returns the frame's slot and every call of
   the sink carries it. The timers and the inter breakdown belong to the slot.
2. **Recording and submission leave the parse thread.** `frame_end` queues the frame; frames are recorded
   and submitted in decode order (the GPU needs the references complete, and its queue is serial). A
   frame that finishes its parse before an earlier one waits in the queue. Show-existing copies go
   through the same queue, so the output order is the order of the bitstream, as now.
3. **Frame buffers are bound at submission.** A picture handle gets its frame buffer when the frame is
   recorded, not when its parse starts: a frame in parse holds parse data only.
4. **References are counted by the host as well.** rav1d drops a picture when a later frame's parse
   overwrites its slot, which can be before the GPU work that reads it is even recorded. `frame_begin`
   takes a reference on the frame's seven references and on its own picture; the completion thread
   returns them behind the frame's fence. A frame buffer goes back to the pool when both rav1d and the
   host have let go, which is today's rule with the roles renamed.
5. **Errors.** A frame that fails returns its slot and is not submitted; rav1d marks the picture, and
   frames that depend on it fail or are decoded as rav1d decides. No `longjmp` remains (`host_entry.c`
   goes).
6. **Flush** (seek, end of stream): `rav1d_flush`, then the host's drain.

### Slots and memory

A slot holds what one frame's parse writes and its command list reads. At 4K 10-bit with the low-memory
parts: mode info share 27.9 MiB, coefficient share 23.9, transform block records 12.0, the block maps
7.2, block indexes 6.0, wavefront grid 4.2, grid 4.0, CDEF, loop restoration and constants 3.3: 88.5
MiB. With the frame buffer every slot above two adds (24.2 MiB) that is the 110 MiB a third slot costs on
the console (663.4 to 773.5 MiB of application memory, limit 1024).

- **One frame in parse (F = 1):** slots as today, two or three. Nothing changes in the D3D12 buffers. The
  C parser's host memory goes; rav1d's parse state comes, an estimated 5 MiB per frame context at 4K plus
  about 1 MiB per reference picture (motion vectors and segmentation map) once the buffers of
  [Kept and dropped](#kept-and-dropped) are gone. rav1d unpatched takes 76 MB per frame context (largest
  resident size 160 MB with two, 618 MB with eight, in the parse-only runs).
- **F frames in parse:** F slots for them and at least one for the frame on the GPU; two keep CPU and GPU
  from waiting for each other (step 2 of [gpu-av1-decoder.md](gpu-av1-decoder.md)). F = 2 means four
  slots: about 884 MiB of application memory at 4K 10-bit. That is too much next to an application.
- **A pool instead of shares** removes the problem: mode info, coefficients and transform block records
  of all frames in flight come from one pool each, in pieces as the parse proceeds. A frame takes what it
  uses (4 to 6 MB on average on the 40 Mbps clips, 17 MB for the largest frame seen) instead of 64 MiB
reserved. A parse that finds the pool
  empty waits for the GPU to finish an older frame; one frame of the worst case must fit, as two shares
  must today. The shaders already address these buffers by offset; the dispatches that run over a tile's
  records become dispatches per piece. This is a step of its own, after frame parallelism works with
  slots.

### How many threads

Projection to the console. The current parser takes 44 ms per frame there on `j4k40` and 34 ms on
`t40x1ld` (31.7 to 36.3 over runs), 0.85 and 0.77 of what it takes on the development machine in the
sustained state. With the measured ratios:

| Clip | Current parser, console | rav1d parse pass, projected | With the sink | p95, projected |
| --- | ---: | ---: | ---: | ---: |
| `j4k40` | 44 ms | 18.5 ms | about 19.5 ms | about 27 ms |
| `t40x1ld` | 34 ms | 15 ms | about 16 ms | about 27 ms |

The projection assumes that the ratio carries over from a Comet Lake core to a Zen 2 core (both run the
AVX2 and SSE2 paths of `msac.asm`); take it as 15 to 23 ms for `j4k40` until the console has measured it.

- **One worker thread reaches 4K30 on the CPU side**: 19.5 ms per frame is 51 frames per second of
  parsing, and the 95th percentile stays under 33.3 ms. Recording (about 1.6 ms) and submission are no
  longer on that thread.
- **A second worker and a second frame context** raise the rate by 1.5 to 1.8 on these streams and cover
  runs of large frames. They are the reserve for goal 2, and they cost a slot until the pool exists.
- **More than three frame contexts gain nothing** on four of the five clips; `t40x1ld` gains nothing
  beyond two.
- Tiles are served by the same workers: `fg4k_300` (8 tile columns) parses in 13.7 ms of CPU time per
  frame, spread over as many workers as there are.

Sizing to the target (goal 3): the settings of the decoder get the number of worker threads and of
frames in parse, with defaults by frame size: one and one up to 1440p, two workers at 4K, two frames in
parse at 4K when the memory guard allows the slot (always, once the pool exists). Idle workers are
parked by rav1d's scheduler and cost nothing. The work itself does not grow with threads: at 4K30 and
40 Mbps the parse is about 0.6 of a core (19.5 ms of 33.3), at 12.6 Mbps about a quarter. The current
parser would need 1.3 cores of work at 30 fps on `j4k40`, on one thread.

### What 4K30 then depends on

Measured on the console with `-gputime` (2026-11-12, the C++ reference package 0.2.117.0 and the two Rust
packages, two runs each, ms per frame, mean):

| Clip | Decode thread | of it, parser | Other decode-thread work | GPU busy |
| --- | ---: | ---: | ---: | ---: |
| `j4k40` | 50.3 | 42.6 | 7.7 | 18.5 |
| `t40x1ld` | 43.5 | 34.7 | 8.8 | 12.1 |

The GPU is busy well under 33.3 ms on both, so after phase 2 the decode thread decides: a parse of about
20 ms (projected above) plus the other 8 to 9 ms of the decode thread (setup, memsets, recording) is about
28 to 29 ms, under 33.3 ms with little margin, so the setup and memsets are the next CPU target after the
parser. Earlier figures of about 50 ms (`j4k40`) and 31 ms (`t40x1ld`) GPU time came from
`-gputimedetail`, which adds a timestamp and barrier per stage and inflates GPU time (same clip and
shaders: 49.3 ms with it, 18.5 ms without); they are not the GPU's cost in normal decoding.

## Console and UWP

- **Toolchain.** rav1d builds on stable (1.99.1 here) for `x86_64-unknown-linux-gnu` and, checked with
  `cargo xwin build --release --lib --target x86_64-pc-windows-msvc`, for Windows; its
  `rust-toolchain.toml` pins a nightly for its own development only, and a build inside rav1g's workspace
  does not read it. Edition 2021 inside an edition 2024 workspace is fine.
- **Assembly.** `build.rs` runs nasm (2.14 or newer) through `nasm-rs` and archives the objects with
  `cc`. The cross build needs nasm in the build image; the Windows runner needs it installed (it is not
  part of the image). Only two files are needed (`msac`, `refmvs`), so the build script of the
  vendored copy lists those.
- **Imports.** rav1d has no file, environment or console access outside logging (`src/log.rs`, a callback
  or standard error) and debug prints that are compiled out; it uses `libc` only for types and error
  numbers. Its threads are `std::thread` (workers named by rav1d, parked when idle), its locks
  `parking_lot`. On Windows `parking_lot_core` 0.9.10 declares five functions of `kernel32.dll` through
  `windows-targets`: `GetLastError`, `CloseHandle`, `GetModuleHandleA`, `GetProcAddress`, `Sleep`; it
  looks up `WaitOnAddress` and `WakeByAddressSingle` at run time and falls back to keyed events of
  `ntdll.dll`, also looked up at run time. In the Windows library the objects of rav1d and
  `parking_lot` name `Sleep`, `CloseHandle`, `GetModuleHandleA`, `GetProcAddress` and
  `WakeByAddressSingle` and nothing else of Win32. All of them are in `WindowsApp.lib`; with the rule of
  step 5 (no import member for a desktop DLL in `rav1g.lib`, `WindowsApp.lib` ahead of the system
  libraries; `testing/uwp_imports`) they come from the API sets. If the console objects, `parking_lot` is
  replaced by the locks of `std` in the vendored copy; that is mechanical. Nothing here touches the
  question of the standard library's two `ntdll.dll` imports or the fallback to the UWP target with
  `-Zbuild-std`: rav1d adds no import of its own kind.
- **Symbols.** The library built here has 1,913 global functions named `dav1d_*` (the assembly, through
  `private_prefix`, and the C API in `lib.rs`). An application that links dav1d statically and rav1g
  statically would have them twice. The vendored copy drops the C API and gives the assembly another
  prefix (`build.rs` and the declarations in `msac.rs`, `refmvs.rs`, `cpu.rs`).
- **Panics.** rav1d returns errors for invalid streams, but it is Rust with bounds checks and
  `unwrap`: a bug shows as a panic. rav1g's workspace unwinds, so the worker loop can catch a panic and
  fail the frame; rav1d's own profile (panic abort) is not used. With the fallback target and
  `-Cpanic=immediate-abort` that is not possible; noted for that decision.
- **Memory.** The parse state per frame context and the removal of the pixel buffers are in
  [Slots and memory](#slots-and-memory). The picture allocator must not allocate.

## Alternatives

- **Tile threads only, with the C parser.** Exists (`-t`). It does nothing for 1-tile streams, which
  three of the five 4K clips are, the heaviest among them, and nothing for the work per frame.
- **A faster single-thread parser by other means.** rav1d's entropy kernels inside the libaom-derived
  parser: the symbol decoder is one part of the difference, the rest is how contexts, coefficients and
  CDFs are laid out, which is the parser. The measured ratio is that of the whole first pass; the design
  takes it whole.
- **dav1d (C) instead of rav1d.** The same algorithms and the same assembly; rav1d's authors report it a
  few percent behind dav1d, not measured here. It would be patched as deeply. Against it: the decoder is
  to end without C, the build is cargo with `cc` only for assembly, the sink can be inlined into the
  parser when both are Rust, and the parser is the part that reads untrusted bytes.
- **The current parser, frame-parallel.** Its state is one `AV1_COMMON` and one current frame in the
  host; frame threads mean per-frame contexts and the CDF and motion vector waits rav1d has. The gain is
  bounded by the same CDF chain (1.5 to 1.8 with two threads) at 2.3 times rav1d's work per frame.

For goal 1 the chosen design wins because it removes the CPU limit with one thread; for goal 3 because
it does less than half the work, needs no second thread for it, and frees the 85,000 lines of C and
their memory.

## Plan

Each step leaves a decoder that works; both parsers are in the program until step 5, chosen at run time
(`rav1g-dec -parser`), so every gate can compare them.

1. **Vendor and build.** `third_party/rav1d` at the pinned commit with its `COPYING`; the patches that
   only remove (C API, pixel code, assembly other than the two files, the symbol prefix). nasm in the
   three build environments (Windows runner, cross build, the application-container library).
   Gate: builds green; the import scan of the application-container library unchanged except for the
   five functions above; on a Linux runner the unpatched rav1d decodes the conformance set to the
   reference md5 (a second software reference next to dav1d, cheap to keep).
2. **Parse-only mode and the sink, with an oracle.** The mode, the sink trait, the change in
   `decode_coefs`. A sink that fills plain memory in the GPU layouts, and a dump of the same buffers
   from the C parser (the stage dumps already write mode info and block lists). Gate: for every frame of
   the conformance set the records (fields that anything reads), the grid, the block lists, the
   transform block records and the coefficients are equal between the two parsers. No GPU involved; runs
   on Linux.
3. **The host on the sink, one frame in parse.** `frame_begin` and `frame_end`, the picture allocator,
   the header module, output and show-existing, errors. Gates: 251 of 251 on WARP with `-parser rav1d`,
   md5 against dav1d and AOM; stage dumps equal to the C parser's on the seven dump streams; the
   "MEMORY" lines; then the console md5 set and `-gputime` against the C parser: tile decode time, decode
   thread, frames per second on the 4K clips.
4. **Frames in parallel.** Frames as values, the ordered submission queue, references counted by the
   host, settings for workers and frames in parse. Gates: the conformance set on WARP with one to four
   frames in parse and one to eight workers, equal md5 in every combination (run repeatedly: the order of
   threads is the risk); console md5 set; console timing with two frames in parse, slots three and four.
5. **The pool** in place of the per-slot shares, and the defaults by frame size. Gates: conformance;
   "MEMORY" lines; console application memory with two frames in parse not above today's three slots;
   a stream that exhausts the pool decodes (the parse waits).
6. **The C parser leaves.** `libav1/av1`, `aom*`, their build in `rav1g-sys`, `host_entry.c`, the
   bindings to parser structs, `-parser`. Gates: everything of CI; console md5 set and timing; licence
   files reviewed.

Console decision points: after step 3 (is the projected parse time real, and is 4K30 reached on
`t40x1ld`), after step 4 (is a second frame in parse worth its memory before the pool exists).

### Risks

- **Byte identity of the translation.** libaom derives some record fields in its own way (the transform
  size of skipped inter blocks, `inter_tx_size` for blocks cut by the frame edge, the vector of global
  motion blocks, chroma positions of blocks under 8x8, the warp validity flag). The oracle of step 2
  finds these before a shader runs.
- **Behaviour around the parser:** operating points and spatial layers, which frame of a temporal unit
  is output, sequence changes with a new size, intra-only frames, show-existing of a key frame, film
  grain seeds. The conformance set has streams for each; the rule for output frames is ported, not
  replaced by rav1d's.
- **The projection.** Zen 2 against the development machine, and the mapped coefficient buffer on the
  console; step 3 measures both.
- **The fork.** The patches touch `decode_b`, `decode_coefs`, `read_coef_blocks`, the task scheduler and
  frame setup. Kept as a list on a pinned commit; updates of rav1d are optional.
- **Thread order.** Out-of-order completion, references released early, errors in one frame while others
  are in parse. Step 4's repeated runs with varied thread counts are the test.
- **Memory** until step 5: two frames in parse at 4K 10-bit only where a fourth slot fits.
- **The decode thread's other work** (about 8 ms per 4K frame: setup, memsets, recording) leaves little margin under 33.3 ms once the parser is about 20 ms; it is not addressed here.

### Estimate

Steps 1 and 2: two to three days. Step 3: three to four. Step 4: two. Step 5: two to three. Step 6: one.
About two working weeks to the removal of the C parser, with the console tests of steps 3 and 4 as the
points where the plan can change.

## Progress

### Steps 1 and 2 (2026-11-13)

Done, on Linux and in the Windows builds; no GPU stage reads anything of the new parser yet. The
decoder is the decoder it was.

**What is in the tree.**

- `third_party/rav1d`: rav1d at `2e7216c`, the parse pass only, behind a sink (`src/parse.rs`):
  67 files, 1.4 MB, 39,100 lines, against 1,364 files upstream (329,000 lines under `src`). 51 files are
  upstream's byte for byte, 12 are changed (+549 −4,852 lines), 4 are ours. `PATCHES.md` there lists
  every change and why; `upstream_diff.sh` holds the directory against a checkout of upstream and writes
  the diff. The changes take away, or hand what is read to the sink; none changes how a syntax element
  is read. The scheduler is upstream's: the reconstruction task of a tile returns at once, the filter
  tasks do nothing, a picture is a handle without samples.
- `crates/rav1g-parse`: `record` (a block of rav1d as a mode info record), `sink` (the sink that writes
  records, grid, transform block records and coefficients into memory a host gives it, and counts the
  block lists with the host's `av1_mi_push_block` at the end of a tile), `digest` (what the parser reads
  from a stream as one number).
- `tools/rav1g-parse-check`: the oracle of step 2, below. Linux only.
- `testing/rav1d_md5`: upstream rav1d, unchanged, as a decoder on the conformance streams.
- The library has rav1d linked: `rav1g_parse_digest_*` in `rav1g.h`, `rav1g-dec -parsedigest`.
- `crates/rav1g-sys` builds on Linux as the entropy decoder alone (no shaders, no D3D12 host), which
  is what the oracle links.

**Step 1, the gates.**

- *Builds.* nasm on the Windows runners through `.github/actions/nasm` (the release of nasm.us, checked
  by its SHA-256, kept in the Actions cache), `apt-get install nasm` in the cross build. rav1d builds
  with the workspace on stable, for MSVC, for clang-cl through `cargo xwin`, and in the two libraries of
  the test player's packages.
- *Imports of the application-container library.* The probe DLL of the cross build (every function of
  `rav1g.h`, `WindowsApp.lib` first) imports the same 98 functions of the same 30 DLLs as before rav1d
  was in it: no line of its import table changed. The functions the list above expected
  (`GetLastError`, `CloseHandle`, `GetModuleHandleA`, `GetProcAddress`, `Sleep`,
  `WakeByAddressSingle`) were already imported by the standard library. Two imports did appear at first,
  `GetSystemInfo` and `ceil`, both from rav1d choosing its own number of threads (the number of
  processors and its square root); the copy now takes the numbers from the application, and they are
  gone. The same holds for the test player's packages: the import tables of `rav1g.dll` and the player
  of the default package (95 functions of 29 DLLs, and 284 of 36) and of the player of the MSVC package
  (305 of 48) are line for line those of the run before. `rav1g.dll` grows from 3,674,112 to 4,382,720
  bytes.
- *Upstream rav1d on the conformance set.* `testing/rav1d_md5` builds upstream's crate at the commit
  from its repository and compares the md5 of every output frame with the references (AOM's md5 files,
  and dav1d's through ffmpeg for the others). CI job `rav1d`, on a Linux runner: 252 of 252 streams,
  1,045 frames. The same job runs `upstream_diff.sh`.

**Step 2, the oracle.** `rav1g-parse-check` runs both parsers on a stream, temporal unit by temporal
unit, each into plain memory laid out as a pipeline slot of the D3D12 host lays it out
(`av1_setup_macroblockd`): the entropy decoder of libaom with a host that has no GPU (the functions of
`dx/av1_core.h`, about 20), rav1d with the sink. The block lists of both are counted by the host's own
`av1_mi_push_block`, compiled into the program from `crates/rav1g/src/host/blocks.rs` as it is. Then
every buffer of a frame is compared over its whole size: the mode info pool record by record and field
by field, the grid as record indexes, the transform block records, the coefficient buffer (it starts
as a pattern, so a coefficient one side writes and the other does not is seen), the block index list,
the block maps, the wavefront grids of intra prediction, the tiles' cursors and counters, the palette
blocks and the loop restoration units. CI job `parse-check`, with one and with four parser threads.

What it found, and how the sink reproduces libaom:

| What | libaom | rav1d | In the sink |
| --- | --- | --- | --- |
| `partition` of the four 4x4 blocks of a split 8x8 | `PARTITION_NONE`: no partition is read under 8x8 | the split of the 8x8 | a split becomes none (it only occurs there) |
| `ref_mv_idx` | counted from the second entry of the list for modes with a NEARMV | counted from the first for every mode | one less for NEARMV, NEAR_NEARMV, NEAR_NEWMV, NEW_NEARMV |
| Coefficients that are zero | not written: the buffer is zero where a frame did not write (the transform stage clears what it read) | the sink's memory, 32 bits each | not written either |
| Coefficient values | dequantised, masked to 24 bits, not clamped to the bit depth | clamped | the clamp is removed in the copy (`recon.rs`) |
| Count in a transform block record | coefficients up to the last, rounded up to four | index of the last | `(eob + 4) >> 2` |
| Transform size of a skipped inter block, of a lossless block | `read_tx_size`: the largest of the block (4x4 if lossless, or in a frame of 4x4 transforms), written to all 16 entries of `inter_tx_size` | the block's `max_ytx`, split bits unused | the same rule from block size and segment |
| `inter_tx_size` of a block with a coded tree | written leaf by leaf; entries of the part outside the frame are never written; `tx_size` is the last leaf | two words of split bits | the tree is walked as `read_tx_size_vartx` walks it, from the split bits; entries outside the frame stay zero |
| CDEF index | in the record that the top left 4x4 unit of each 64x64 points to; −1 if all its blocks are skipped; not written in a lossless or intra-block-copy frame | per 64x64 in the loop filter masks | written through the grid at the end of the superblock |
| Lossless 4x4 transform | `DCT_DCT` with its own counter (`TX_SIZES_ALL`) | `WHT_WHT` | mapped |
| Filter intra | DC mode with the filter next to it | a mode of its own | mapped |
| Chroma from luma | index and joint sign | two signed values | computed back |
| Reference of an inter-intra block, of a single reference | second reference `INTRA_FRAME`, `NONE_FRAME` | a flag | mapped |
| Local warp that cannot be fitted | `wm_params.invalid` | the model's type stays identity | the mark is set, the numbers are not written (see below) |
| Palette index map | extended to the whole block: columns and rows outside the frame repeat the last one inside | columns filled, rows not | the host of the sink repeats the last row |

Equal without any work: the vector of a block with a global motion mode (rav1d stores the vector
libaom derives from the model), the positions of chroma transform blocks of blocks under 8x8, the
warp flag of a reference under global motion (`gm_warp` of the slot, against rav1d's
`gmv_warp_allowed`), the delta of the loop filter and the quantiser index of a block, segment ids,
compound types and masks, the interpolation filters, intra block copy, loop restoration units.

Two things are not compared. `num_proj_ref`, the number of samples libaom found for a local warp:
nothing behind the parser reads it. The numbers of a local warp that libaom marked as not valid: the
fit stopped somewhere and the block is predicted from its vector; the mark itself is compared. **No
stream we have contains such a block** (0 of 470,000 local warps), so that mark is the one field the
oracle has not exercised.

Results. In CI, on a Linux runner, the 252 streams of the conformance set are equal in all 1,108
frames, with one parser thread and with four: 1,241,146 records, 1,483,395 transform blocks,
44,829,632 coefficient slots, 477 palette planes, 6,559 loop restoration units. Among the records:
768,000 inter blocks (481,000 skipped, 184,000 with transform sizes that are not all one), 202,000
compound (44,000 with a wedge or difference mask), 31,000 inter-intra, 132,000 with overlapped motion,
42,000 local warps, 11,600 with a global motion mode (4,900 warped), 17,000 intra block copies, 354,000
blocks that are 4 wide or high, 10,700 that cross the frame edge.

Locally, beyond that set, these are equal too: 13 small streams encoded
for the purpose with libaom, rav1e and SVT-AV1 (segmentation, loop filter deltas, lossless, screen
content with palettes and intra block copy, monochrome, 128x128 superblocks with tiles, error
resilient; 330 frames), and longer clips (a 1080p clip of 14,645 frames with 19 million records, 4K
clips of 300 frames, a 4K clip with 40x8 tiles; the small streams and the tiles clip also with four
parser threads). Among the records compared: 4.7 million compound blocks (1.4 million with a wedge or difference mask), 1.2
million inter-intra, 3.9 million with overlapped motion, 470,000 local warps, 260,000 with a global
motion mode (46,000 of them warped), 5.2 million inter blocks whose transform sizes are not all one,
480,000 blocks that cross the frame edge, 23,500 in a segment that is not 0, 1,600 with a loop filter
delta, 17,000 intra block copies. The conformance set itself has no block in a segment other than 0
and none with a loop filter delta: those two come from the streams encoded for the purpose, which are
not in CI.

What the entropy decoder of libaom needed from the plain host, and so from the D3D12 host today (step
3 has to give the new parser the same, or not need it):

- The frame buffer description of a frame with super-resolution is rewritten to the upscaled width
  after the frame (`postfilter.rs`); libaom takes the size of a reference from there. Without it the
  parse of a later frame goes wrong. rav1d keeps this itself.
- Per frame buffer, the motion vectors and the segment map of the frame in it; the context arrays above
  a tile row; the loop restoration units; the motion field. rav1d owns all of these.

Found on the way: `end()` of `aom_util/aom_thread.c` joined a thread that was never started (the
worker that runs on the calling thread), which the Windows emulation of pthreads tolerates and glibc
does not; it now ends only workers that have a thread. Two `#include` lines of the entropy decoder had
backslashes. The bindings give a C enum the type `int` on every target (a Linux compiler takes
`unsigned int`; the host is written against the Windows type).

On damaged streams the two parsers do not behave alike, as expected: libaom reports the frame and goes
on with the next temporal unit, rav1d returns an error for the unit and drops the rest of it (seen on
the cut-tile streams; the frames both parse are equal). What an application sees for a damaged stream
is decided in step 3.

**The digest.** `rav1g_parse::digest` folds what the parser hands the sink (the record of every block,
transform blocks, coefficients, palettes, CDEF indexes, loop restoration units) into a 64-bit number
per stream. `rav1g-dec -parsedigest` prints it in the Windows builds, `rav1g-parse-check -digest` on
Linux, and CI job `parse-digest` requires the same lines from the MSVC build, the Linux cross build
(both with four threads) and the Linux build (one thread). It is the cheap check that the parser,
with its assembly, runs the same in every build; the values are checked on Linux.

**CPU cost of the sink** (`rav1g-parse-check -time`: the stream parsed with a sink that keeps
nothing, and with the sink writing the buffers; one thread, best of three, the difference is the
sink). On the development machine, 3840x2160, milliseconds a frame:

| Clip | Parse alone | With the sink | Sink | of which block lists |
| --- | --- | --- | --- | --- |
| `a1_8b4k_300` (light) | 4.7 ms | 5.3 ms | 0.6 ms | 0.2 ms |
| `fg4k_300` | 8.1 ms | 9.0 ms | 0.8 ms | 0.3 ms |
| `j4k40` (heavy) | 14.6 ms | 16.2 ms | 1.6 ms | 1.5 ms |
| `t40x8` (40x8 tiles) | 11.8 ms | 11.9 ms | 0.1 ms | 0.5 ms |

The difference of two runs is noisy at this size (the last row has a sink smaller than its block
lists), but the estimate of [CPU cost of the hand-off](#cpu-cost-of-the-hand-off) holds: about 0.5
ms on a light 4K frame, 1.5 ms on a heavy one. Most of it on a heavy frame is `av1_mi_push_block`,
which the tile threads of the entropy decoder run today as well; the records, the grid and the
transform block records are the smaller part.

On the Linux runner of CI (`parse-check`, the two largest streams of the set): the 4K stream
`nf-dvbdash-a1-8b-2160p`, 23 frames, parse 5.7 ms, with the sink 7.6 ms, the sink 1.9 ms (block lists
0.4 ms); `nf-chimera-10b-1080p`, 33 frames, parse 3.7 ms, with the sink 4.4 ms, the sink 0.75 ms (block
lists 0.3 ms). That 4K stream is the source of `a1_8b4k_300` above, where the sink is 0.6 ms: the runner
is the slower machine for this, and 23 frames are few. What is left when the block lists are taken off
is mostly memory: the grid alone is one pointer per 4x4 unit, 4 MB written for a 4K frame, which the
entropy decoder of libaom writes as well.

**What step 3 starts from.**

- `ParseSink` and `rav1g_parse::sink::Sink` with its `SinkHost`: the D3D12 host implements `SinkHost`
  on the mapped buffers of a slot (`frame_begin`, `tile_begin` with the cursors of
  `av1_setup_macroblockd`, `palette` in place of `av1_intra_palette`, `superblock` for the loop
  restoration units, `tile_end`, `frame_end` in place of `av1_decode_frame_gpu`).
- The sink calls `av1_mi_push_block` through structs of the entropy decoder that it fills itself (an
  `AV1Decoder` with the frame's size, grid and superblock size; a `MACROBLOCKD` per tile). That works
  unchanged with the real host; a block context of the host's own is step 6.
- One frame is in parse at a time (`Sink` keeps one frame's state); the tiles of a frame may be parsed
  side by side.
- The sink leaves the second share of a slot alone: the oracle gives a tile room for all its blocks and
  the worst-case coefficient layout. The real host has to switch shares as `av1_setup_sec_data` and
  `av1_setup_ext_coef_buffer` do, or size the parts for the worst case (step 5 replaces the shares).
- Still to come from rav1d for the host, and not part of the sink yet: the frame header in the host's
  terms (quantiser matrices, loop filter levels, CDEF and loop restoration parameters, film grain,
  reference list and scale: `ParseFrame` has the headers), the picture allocator, output order and
  show-existing, operating points, errors.
- The warp validity mark needs a stream that has such a block.

**Runs.** Commits `85f6fbe` (rav1d as published), `5632c6c` (the changes to it), `e5e9e42` (the
workspace, the sink, the oracle, CI), `159fb21` (this section). CI run 43429728285 on `159fb21`, all 17
jobs green: 252 of 252 on WARP with `rav1g-dec` of the MSVC build and of the cross build, also through
`rav1g.h`, with `-poisonmi`, `-poisonbuf` and `-interfast`; order check, cut-tile, player smoke, memory
check, the equality with the C++ decoder; no device removal; the import checks of both packages and of
the application-container probe; `rav1d` 252 of 252; `parse-check` 252 of 252 with one and with four
threads; `parse-digest` the same 252 lines from the three builds. The sink on that run's runner: 1.8 ms
on the 4K stream, 0.6 ms on the 1080p stream. Before it, on a throwaway branch: run 43426711444 (the
whole workflow; everything green but the build of `parse-check`, where bindgen loaded a libclang of
another version than the headers clang gave it) and run 43429200887 (that job alone, with one LLVM
version as in the cross build).

### Step 3 (2026-11-13)

Done: the D3D12 host runs on the sink, one frame in parse at a time. A decoder is created with one of
the two parsers and keeps it; the entropy decoder of libaom is the default until the console has timed
both. (It has: the default is rav1d now, and the table below is the state of step 3. See
[Console after step 3](#console-after-step-3-rav1d-is-the-default).)

| Where | The choice |
| --- | --- |
| Rust | `Settings::parser`: `Parser::Libaom` (default) or `Parser::Rav1d` |
| C (`rav1g.h`) | `Rav1gSettings::parser`: `RAV1G_PARSER_LIBAOM` (default) or `RAV1G_PARSER_RAV1D` |
| `rav1g-dec` | `-parser c` (default) or `-parser rav1d`, per task |
| Test player | `-parser c` (default) or `-parser rav1d`, per task line ([test-player.md](test-player.md#parser--parser)) |

**What is in the tree.**

- `crates/rav1g/src/host/rav1d_host.rs`: the host of the sink (`SinkHost` on the mapped buffers of a
  slot) and the decode call of such a decoder. `aom_codec_dec_cfg_t::parser` selects it; the decode call
  of the C interface (`decoder_decode` of `av1/av1_dx_iface.c`) hands the temporal unit to it instead of
  to the entropy decoder. Everything else of a decoder is the same code: creation, slots, frame buffers,
  recording, submission, the completion thread, the drain.
- The stage modules read a frame's header through the structs of the entropy decoder. The new host
  keeps such structs of its own, cleared, and writes into them what the stages read (about 60 fields)
  from the headers rav1d has parsed; it then calls the functions the entropy decoder calls. A field
  libaom does not write for a frame is not written either, so the structs hold the same values frame
  for frame, also the ones that are left over from an earlier frame. The stages do not see which parser
  ran, and no stage module changed. This is the "small module" of [Frame level](#frame-level); it goes
  with the C structs in step 6.
- The sink (`crates/rav1g-parse/src/sink.rs`) switches to the second share of a slot as the entropy
  decoder does: a tile whose part of the mode info pool is full goes on in the part of the frame's second
  slot, and a tile whose coefficients have reached the mark of their part goes on there at the next
  superblock (`av1_setup_sec_data`, `av1_setup_ext_coef_buffer`, now also as Rust functions of
  `frame.rs`). A frame takes a second slot under the same conditions with both parsers.
- `third_party/rav1d`: the sink is told about a sequence header (and can refuse it), about a frame
  header that shows a reference slot, and where the OBU of a frame lies in the data; it says at the end
  of a frame whether the frame stands; the parse of a tile is timed for it. `PATCHES.md` has the list
  (13 files changed, 50 as upstream).

**How each duty of the host is done.**

| Duty | With the entropy decoder of libaom | With rav1d |
| --- | --- | --- |
| Frame begin | `av1_setup_frame`: a slot, the clears, the frame's buffers and references counted | the same three functions, from `frame_begin` of the sink, on the thread of the decode call |
| Tiles | `av1_setup_macroblockd` per tile thread | `tile_begin` of the sink, the same function, on rav1d's thread for the tile |
| Palette | `av1_intra_palette` | `palette` of the sink into the same writer; the rows of a block outside the frame repeat the last one inside |
| Loop restoration units | the entropy decoder writes `rst_info[].unit_info` | `superblock` of the sink writes the same array (the host memory the decoder has for it) |
| Frame end | `av1_decode_frame_gpu`: record, output image, submit | the same two functions from `frame_end`; the time between `frame_begin` and `frame_end` is `entropy_wall` |
| Frame buffers | libaom's buffer pool calls the host for a buffer and when it lets go | the host keeps the eight reference slots itself, as libaom does: a frame that stands refreshes its slots, a key frame shown by `show_existing_frame` takes all, a new sequence empties them. rav1d's pictures are handles without samples and are not looked at |
| Output order | the completion thread hands frames over in the order of submission | the same: frames are submitted in decode order |
| Which frame of a temporal unit is output | `av1_frame_is_output`, a look ahead over the OBU headers behind the frame | the same function, asked with the place of the frame's last OBU in the data of the call (rav1d's own rule and picture queue are not used) |
| `show_existing_frame` | `av1_decode_sef`: an output copy with the film grain of the shown frame | the same two functions from `show_existing` of the sink, with the film grain parameters the host kept for the frame |
| Operating point, all layers | `AV1D_SET_OPERATING_POINT`, `AV1D_SET_OUTPUT_ALL_LAYERS` | the operating point goes to rav1d when the parser is made (the first decode call) and decides which OBUs are parsed; all layers is libaom's output rule |
| A sequence the decoder was not created for | `AOM_CODEC_NEEDS_RECONFIGURE` before anything of it is decoded | the sink refuses the sequence header with the same code and the same text; rav1d keeps the state it had |
| Superres | the host rewrites the frame buffer's width to the upscaled one, which the entropy decoder reads for later frames | the same rewrite, for the host's own stages; rav1d keeps the sizes of its references itself |
| Film grain | `cm->film_grain_params`, kept per frame buffer | from rav1d's header, in libaom's numbers (the chroma multipliers and offsets as coded), kept per frame; a frame that takes the parameters of a reference gets what was kept for that reference |
| Global motion | the models with their shears | the matrices of rav1d's header through libaom's `get_shear_params` |
| The slot's second share | `av1_setup_sec_data`, `av1_setup_ext_coef_buffer` | the sink, at the same points (above) |
| Timers | `entropy_sum` per tile thread, `entropy_wall` around the tiles | `entropy_sum` around every superblock row of a tile, `entropy_wall` from the end of the frame's set-up to the start of its recording: both have the sink inside, and rav1d's set-up and wrap-up of the frame's parse |

Only 4:2:0 and monochrome sequences are taken with rav1d (the sink and the shaders are for those); the
entropy decoder reads other layouts and the stages then decode them wrongly.

**A damaged stream.** The two parsers do not do the same, and step 3 keeps rav1d's rule:

- The frame whose parse fails returns its slot and its buffers and is not submitted, and the decode
  call returns the error (`Corrupt frame`, "Failed to decode tile data: the parse of the frame ended
  with an error"). The rest of its temporal unit is dropped. This is the same with both parsers.
- With the entropy decoder of libaom the reference slots stay as they were before the frame, and the
  frames that follow are decoded from them: wrong pictures, or further errors when the wrong state does
  not parse, until a key frame.
- With rav1d the slots the failed frame was to refresh are empty afterwards (rav1d's own bookkeeping,
  and the host's map does the same). A later frame that names an empty slot fails before anything of it
  is set up: the decode call returns `Corrupt frame`, "The temporal unit could not be parsed
  (InvalidArgument): ...", and that frame's own slots stay as they were. A frame whose references are
  all whole is decoded.
- With both, decoding is right again from the first frame whose references are whole, at the latest
  the next key frame, and the frames before the damage are right.

Why rav1d's rule and not libaom's: with frames in parallel (step 4) the references of a failed frame
cannot be put back, because later frames have already taken them; "a frame that depends on a failed
frame fails" is a rule the host can keep there, and it never gives the GPU a frame that was parsed
against the wrong references. For an application it means missing frames instead of wrong ones, each
with an error from the decode call. The checks keep their meaning: the cut-tile check
(`testing/conformance/cut_tile_check.py --parser rav1d`) holds the same things as with the C parser (the
task ends with the error and has the frames before the damage; the stream behind it is right; with
`-continue_on_error` the decoder goes on, the frames before the damage and from the next key frame on
are those of the reference; no lost device), with rav1d's words for the error. Measured on the two
streams of that check and on the one-tile stream followed by itself whole: the frames from the next key
frame on are the same with both parsers, frame for frame.

Two things had to be right for this. `Parser::send` of the copy now leaves nothing of the data with
the parser after an error: upstream's `gen_picture` keeps unread data when a picture is ready, also
behind an OBU it could not read, and the next call then read that OBU again in front of its own data
and failed, key frame or not. And the sink's verdict on a frame reaches rav1d (`frame_end` returns
whether the frame stands), so that a frame the host could not take (a tile without room in two shares)
is a failed frame in rav1d's bookkeeping as well.

A panic in rav1d (a bug, not a damaged stream) is caught at the decode call when the parse runs on its
thread: the call returns an error and the decoder decodes nothing more. With more than one parser
thread a panic on a worker thread is not caught yet (step 4).

**Gates** (CI run 43447155433 on `a2fb4be`, all 17 jobs green; WARP):

| Gate | With `-parser rav1d` |
| --- | --- |
| Conformance set, md5 against dav1d and AOM | 252 of 252 with `rav1g-dec` of the MSVC build and of the cross build, four threads |
| The two parsers against each other | md5 per output frame: 252 of 252 streams equal, 1,045 frames |
| Stage dumps of the seven dump streams | every frame buffer behind every stage and every list the stages read equal (block lists, loop filter lists and parameters, CDEF and loop restoration lists, the info line); the mode info records equal in what a stage reads of them, see below |
| MEMORY lines | the same as with the C parser in all 10 cases |
| Through `rav1g.h` (`-capi`) | the part of the set, and the cut-tile check |
| Cut-tile check | the program, the program through `rav1g.h`, the player's decoder code; MSVC build and cross build |
| Order check, memory check | both builds; 40 decoders md5 pass, video memory back at the start after every release |
| `-poisonmi`, `-poisonbuf` (part of the set), `-interfast` (whole set) | both builds |
| Player smoke with `-parser rav1d` | 4 of 4 tasks: MD5 is OK, both builds |
| `D3D12 DEVICE REMOVALS` | 0 in every run |
| Everything that existed, with the C parser | green, with the equality to the C++ decoder (`rust-equal`) |
| Import checks | unchanged: `rav1g.dll` 95 functions of 29 DLLs, the player of the default package 284 of 36, the player of the MSVC package 305 of 48 |

The mode info records in the dumps. The comparison of the two parsers (`stage_dump.py compare
--two-parsers`) lists the records and does not hold them equal byte for byte, for two reasons that are
not the new parser's: the entropy decoder of libaom does not clear a record, so a field the block's
kind does not have (the filter intra mode of an inter block, the warp model of a block without one, the
CDEF index of a record that is not the first of its 64x64, ...) holds what a frame before left at that
place of the pool, and it keeps `num_proj_ref`; the sink clears the record. On the first two frames of a
decoder the records differ in `num_proj_ref` alone; later in such left-over fields. No stage reads
them: what the stages make of the records (`pred_blocks`, `lf_blocks`, `cdef_info`) and every frame
buffer are equal, and the runs with `-poisonmi` pass with both parsers. Reproducing the left-over bytes
would mean writing each field only where libaom writes it; that was not done.

Found on the way to green: the superres denominator in a sequence without superres (libaom does not
write it; the info line of a dump printed 0 against 8), fixed in the host.

**Timing on CI.** (Since 2026-11-13 not a step of a push: the job `parser-timing`, once a week and
when the workflow is started with the `parser_timing` input,
[warp-decoder.md](warp-decoder.md#paired-timing-of-the-parsers).)
`testing/conformance/parser_timing.py`: `rav1g-dec -gputime` with `-parser c` and
`-parser rav1d` in turns on one runner, three rounds each, milliseconds a frame (the mean of the rounds'
means). WARP is a CPU rasteriser on the same cores, so `setup` is the wait for a slot and `frame` is
mostly that; the parse is what compares. One parser thread, except the last stream (eight tiles, four
threads). Run 43447155433:

| Stream | Timer | `-parser c` | `-parser rav1d` | Ratio |
| --- | --- | ---: | ---: | ---: |
| 2160p 8-bit (`nf-dvbdash-a1-8b-2160p`, 15 frames timed) | `entropy_sum` | 12.89 | 8.96 | 0.70 |
| | `entropy_wall` | 12.90 | 9.14 | 0.71 |
| | `setup` | 263.6 | 268.1 | 1.02 |
| | `record` | 0.053 | 0.052 | 0.99 |
| | `frame` | 276.8 | 277.7 | 1.00 |
| | decode call | 379.8 | 374.3 | 0.99 |
| 1080p 10-bit (`nf-chimera-10b-1080p`, 25 frames) | `entropy_sum` | 9.52 | 7.39 | 0.78 |
| | `entropy_wall` | 9.53 | 7.95 | 0.83 |
| | `frame` | 593.5 | 591.4 | 1.00 |
| 1080p 10-bit (`nf-sparks-10b-1080p`, 36 frames) | `entropy_sum` | 5.76 | 4.32 | 0.75 |
| | `entropy_wall` | 5.77 | 4.71 | 0.82 |
| | `frame` | 391.8 | 392.1 | 1.00 |
| 1080p 10-bit, 8 tiles, 4 threads (`syn-tiles-10b-1080p`, 32 frames) | `entropy_sum` | 9.07 | 5.20 | 0.57 |
| | `entropy_wall` | 5.69 | 4.61 | 0.81 |
| | `frame` | 120.6 | 119.6 | 0.99 |

- With rav1d `entropy_sum` has the sink inside (on this runner about 1.9 ms of a 4K frame, step 2), and
  `entropy_wall` also rav1d's set-up and wrap-up of a frame's parse (0.2 ms at 4K, 0.4 to 0.6 ms at
  1080p 10-bit, where it is larger than the C parser's).
- The 2160p stream is the light one of the design's measurements (`a1_8b4k_300`, ratio 0.70 there, the
  same here). The heavy 4K clips, where the design measured 0.42 to 0.44, are not in the conformance
  set: the console decides, with `-parser c` and `-parser rav1d` on the same clip in one launch.
- With four threads on eight tiles the work is 0.57 of the C parser's and the wall time 0.81: rav1d
  parses a tile row as superblock rows that take turns, and the sink's block lists come at the end of
  each tile.

**Open for step 4.**

- Frames as values: the host of the sink and the sink itself keep "the frame in parse" (the slot in
  `curr_frame_data`, the header structs, the tiles' state); `frame_begin` has to hand out a frame, and
  recording and submission have to leave the parse thread, in decode order.
- The reference slots are the host's own already; with frames in parallel a frame's slots have to be
  marked at `frame_begin` and a failed frame has to fail the frames that took it (the rule of "A damaged
  stream"), which rav1d does not do by itself with more than one frame in parse.
- A panic on one of rav1d's worker threads is not caught.
- The operating point is fixed when the parser is made (the first decode call); a change after that is
  not passed on.
- The header structs of the entropy decoder that the new host fills (1.2 MB each, one in the sink and
  one in the host) and the C parser's own state, which a decoder with rav1d still allocates, go in
  step 6.
- The sink's cost (1.9 ms of a 4K frame on the CI runner) is mostly the grid and the records; clearing
  each record before it is filled is part of it and could go where every field is written.
- The warp validity mark is still not exercised by any stream we have.
- The player's packages of this run for the console timing: `uwp_dx12_player_0.2.136.0_x64.msix`
  (default, `rav1g.dll` with the C of clang) and `uwp_dx12_player_0.2.136.2_x64.msix` (C of MSVC), with
  `-parser rav1d` on a task line.

**Runs.** Commits `72c9212` (the host on the sink, the settings, the checks, the notices, the lifecycle
lines) and `a2fb4be` (the comparison of the two parsers' dumps, the superres denominator). CI run
43440279847 on `72c9212`: everything green except the comparison of the two parsers' stage dumps, for
the two reasons above. CI run 43447155433 on `a2fb4be`: all 17 jobs green.

**Licence notices.** `third_party/rust-crates/NOTICES.txt` has the MIT and Apache-2.0 notices of the 32
crates of crates.io that are linked into the library and `rav1g-dec` (rav1d's dependencies and the
Windows bindings), written by `collect.py` there from `Cargo.lock`; CI holds the file against the lock
file and copies it into the `licenses` directory of every package.

**The test player** logs its lifecycle events, one `LIFECYCLE ...` line each
([test-player.md](test-player.md#inputs-and-logs-in-localstate)).

### Where the time goes

2026-11-13. The console timed step 3, the ratio was not the one the design measured, and this section
is the search for the reason: what the console said, what was looked at, what was found, and the
switches and timers that are in the tree so that the console can say which of it counts. What the
console then said, and what became of the switches (`nolfmask` and `tilethread` are the default,
`stage` is gone), is in [the next section](#console-after-step-3-rav1d-is-the-default); this one is
left as it was written.

**The console on step 3.** Package 0.2.136.0, `-t 6 -queue_size 4 -nopresent -gputime`, two runs
each, milliseconds a frame, means. The md5 set is 15 of 15 with `-parser rav1d`; the GPU is as busy
with either parser.

| Clip | Timer | `-parser c` | `-parser rav1d` | Ratio |
| --- | --- | ---: | ---: | ---: |
| `j4k40` (4K 10-bit, 1 tile) | tile decode, wall | 40.99 | 30.53 | 0.74 |
| | tile decode, thread sum | 40.95 | 29.19 | 0.71 |
| | decode thread | 48.27 | 39.53 | 0.82 |
| | wall between frames | 50.47 (19.8 fps) | 41.12 (24.3 fps) | |
| `t40x1ld` (4K 10-bit, 1 tile) | tile decode, wall | 33.88 | 23.77 | 0.70 |
| | decode thread | 42.35 | 32.64 | 0.77 |
| | frames per second | 22.6 | 29.3 | |
| `fg4k_300` (8 tiles) | tile decode, wall | 6.50 | 6.38 | 0.98 |
| | tile decode, thread sum | 28.70 | 27.78 | 0.97 |
| `a1_8b4k_300` (4K 8-bit, light) | tile decode, wall | 10.09 | 13.70 | 1.36 |
| | tile decode, thread sum | 10.06 | 12.71 | 1.26 |
| | decode thread | 16.81 | 20.87 | 1.24 |

So 0.71 to 0.74 on the heavy clips, where the design measured 0.42 to 0.44 for the parse pass alone,
and slower than the C parser on the light clip. What the decode thread does besides the parse stayed
at 7 to 9 ms. The eight-tile clip is bound by the GPU and says nothing here.

**The first suspicion: scattered writes into write-combined memory.** Not confirmed by the code. What
the parse writes into mapped buffers of the D3D12 host, with either parser:

| Buffer | Kind of the buffer (`gpu/d3d12.rs`) | Memory for the CPU | Written by | How |
| --- | --- | --- | --- | --- |
| `idct_coefs`, the coefficients | `UploadUAV`: custom heap, pool L0, `WRITE_COMBINE`, unordered access | write-combined | rav1d's `decode_coefs` (C: `av1_read_coeffs_txb`) | the coefficients that are not zero, 32 bits each, in rising scan order within a transform block, the blocks in decode order; never read |
| `idct_blocks_unordered`, the transform block records | `DeviceUpload`: `D3D12_HEAP_TYPE_UPLOAD` | write-combined | the sink's `txb` (C: the same function) | 16 bytes a record, one after the other, every field once; never read |
| `mode_info_pool`, the mode info records | `HostRW`: custom heap, pool L0, `WRITE_BACK` | cached | the sink's `block`, 224 bytes a record, cleared and filled (the C parser fills without clearing) | read again for the CDEF index of a superblock and for the block lists |
| `mode_info_grid` | `HostRW` | cached | the sink's `block`: a pointer at every 4x4 unit of the block | read by the block lists |
| `gen_mi_block_indexes`, `gen_block_map`, `gen_intra_inter_grid` | `HostRW` | cached | `av1_mi_push_block` at the end of a tile, the host's function, for both parsers | counted up |
| `palette_buffer` | `DeviceUpload` | write-combined | the host's palette writer, for both parsers; screen content only | each sample once; never read |

- rav1d's second loop over a transform block walks the coefficients that are not zero from the lowest
  scan index up (the token loop links them that way), and the patch of step 2 stores each at its scan
  index: the addresses rise, with gaps where a coefficient is zero. That is what the C parser writes,
  in the same order. Nothing is written twice, nothing is read back, there is no `|=` on mapped memory
  and no clear of a record in write-combined memory.
- The block lists are counted at the end of a tile from the records by both parsers (`decode_tile2`
  of libaom does the same loop), so that pass over cold records is not new either.
- What differs in the write-combined buffers is only what runs between two stores. Whether that
  matters on the console cannot be seen on a machine whose upload memory is ordinary memory (WARP,
  lavapipe), so there is a switch for it (`stage`, below).

**What was found instead**, on the development machine and on CI:

1. *rav1d hands a tile from thread to thread.* With more than one worker thread the scheduler of
   upstream puts a tile back into its task queue after every superblock row, wakes a waiting thread,
   and the next row is parsed by whichever thread takes the task first. (Upstream has a path that goes
   on with the next row at once. It is never taken: `check_tile` is asked before the progress of the
   row that has just ended is stored, and says "not yet".) For upstream this interleaves the parse with
   the filters of the rows; here the filter tasks do nothing. With `-t 6` on a stream with one tile that
   is 34 hand-overs a 4K frame between six threads of which five have nothing else to do. Parse and
   sink of `j4k40` on the idle development machine: 15.1 ms a frame with one thread, 19.6 ms with six.
   The console was timed with `-t 6`.
2. *rav1d fills the masks of its own loop filter.* The parse pass of upstream computes, block by
   block, the edge masks and the filter levels of every 4x4 unit for its loop filter
   (`rav1d_create_lf_mask_intra`, `_inter`). The loop filter here is a GPU stage with its own lists;
   nothing reads the masks. Their cost goes with the area of the frame and not with its bits, so it is
   what a light 4K frame mostly consists of: 27 % of the parse of `a1_8b4k_300` without the sink (4.09
   to 2.99 ms a frame), and under wine the median frame of that clip takes rav1d 4.0 ms where the C
   parser takes 2.1 ms (below).
3. *The sink is not it.* It costs what step 2 measured: on `j4k40` under wine 1.9 ms a frame for the
   block lists and 1.4 ms for all other calls of the sink, of 20.5.

**The switches.** Three ways of doing the same parse, each a switch of the decoder so that one package
can time them against each other, all off unless asked for:

| `-parseopt` word | `ParserOptions` field, `RAV1G_PARSER_OPTION_*` | What it does |
| --- | --- | --- |
| `stage` | `stage`, `_STAGE` | The sink collects the coefficients and the transform block records of a tile in memory of its own (32 KiB and 8 KiB a tile, so they stay in the first-level cache) and copies them to their buffers in runs: a run ends when the stage is full, when the buffer does not go on where the run ends (the tile's second share), and at the end of the tile. The parser then writes write-combined memory only through those copies: every word of a run, in order. A coefficient that is zero is then written as zero; without the switch it is left as the zero the buffer has. The bytes of the buffers are the same. |
| `nolfmask` | `no_lf_masks`, `_NO_LF_MASKS` | rav1d leaves out the two calls of point 2 (`ParseSettings::lf_masks` off). |
| `tilethread` | `tile_keeps_thread`, `_TILE_KEEPS_THREAD` | The progress of a superblock row is stored before `check_tile` is asked, so a tile whose next row waits for nothing goes on with it on its thread (`ParseSettings::tile_keeps_thread`). With one frame in parse no thread is woken for the row; the filter tasks, which do nothing, run when the tile is through. With more than one frame in parse (step 4) the signal stays upstream's, because other frames wait for rows. |
| `all` | | The three. |
| `time` | `time_calls`, `_TIME_CALLS` | The calls of the sink are timed, see below. |

`Settings::parser_options` (Rust), `Rav1gSettings::parser_options` (`rav1g.h`), `rav1g-dec -parseopt`
and the test player's `-parseopt` ([test-player.md](test-player.md#switches-of-the-parser--parseopt));
`rav1g-parse-check -parseopt` for the oracle and for `-time`. The two switches inside rav1d are in
`PATCHES.md`: the parse is upstream's with both off.

**The timers.** A timing run (`-gputime`) with `-parser rav1d` has three more CPU timers, all parts of
`entropy_sum` and summed over the tile threads:

| Timer | What |
| --- | --- |
| `sink_lists` | the block lists at the end of a tile. Always on in a timing run: two clock readings a tile. |
| `sink_staged` | with `stage`: the copies of the staged runs into the buffers. Two clock readings a run. |
| `sink_calls` | with `time`: the time inside the calls of the sink for a block, a transform block, a palette and a superblock, from the processor's time stamp counter (two readings a call; the counter's length is taken from the clock over the tile). `sink_staged` is inside. |

`entropy_sum - sink_lists - sink_calls` is then rav1d by itself, and without `stage` it still has the
coefficient stores into the mapped buffer in it; with `stage` those stores go to ordinary memory and
the write into the buffer is `sink_staged`. So `stage` against no `stage`, both with `time`, says what
the direct stores cost. The summary prints the three and that difference. The C parser has no such
timers: it does the same work inside its tile decode.

**Measured.**

Parse and sink alone, no GPU (`rav1g-parse-check -time -parseopt ...`, the development machine, idle,
300 frames, the best of three, milliseconds a frame):

| Clip | Threads | No switch | `nolfmask` | `tilethread` | `stage` | `all` |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| `a1_8b4k_300` | 1 | 4.76 | 3.49 | 4.88 | 4.97 | 3.70 |
| | 6 | 5.39 | 4.00 | 5.16 | 5.60 | 3.96 |
| `j4k40` | 1 | 15.07 | 15.47 | 15.39 | 15.75 | 15.81 |
| | 6 | 19.58 | 19.86 | 14.89 | 20.42 | 15.18 |
| `t40x1ld` | 1 | 12.48 | 11.78 | 12.42 | 12.43 | 12.04 |
| | 6 | 14.23 | 16.69 | 12.12 | 17.02 | 11.92 |

- Six threads without `tilethread` cost 4.5 ms a frame on `j4k40` and vary from run to run (14.2 to
  17.0 on `t40x1ld`); with `tilethread` six threads are as fast as one, or faster (the parse then has a
  thread of its own next to the one that sends the data).
- `nolfmask` takes a quarter off the light clip and nothing off `j4k40`, whose time is coefficients.
- `stage` costs 0.2 to 0.7 ms here, where the buffers are ordinary memory: it is a second pass over
  the coefficients.

The decoder under wine on the same machine (`rav1g-dec -hw -gputime -t 6`, the first 80 frames, two
runs, `entropy_sum` in milliseconds a frame; lavapipe renders on the same cores):

| Clip | `-parser c` | `-parser rav1d` | `-t 1` | `nolfmask` | `tilethread` | `stage` | `all` |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| `a1_8b4k_300`, mean | 5.7, 7.2 | 6.3, 6.8 | 6.4, 6.6 | 4.7, 4.6 | 6.8, 7.2 | 6.9, 7.0 | 4.6, 4.8 |
| `a1_8b4k_300`, median | 2.1, 2.4 | 4.0, 4.2 | 3.4, 5.0 | 2.3, 2.2 | 5.2, 5.3 | 4.4, 4.4 | 2.6, 2.7 |
| `j4k40`, mean | 35.6, 34.5 | 20.6, 20.5 | 19.3, 19.8 | 20.1, 20.7 | 21.2, 21.5 | 20.5, 20.1 | 21.7, 22.2 |

- The light clip reproduces the console: without the switches the median frame takes rav1d twice the
  C parser's time, and `nolfmask` brings it to the C parser's.
- `tilethread` gains nothing here and `entropy_sum` rises by 0.7 ms on `j4k40`, while the time around
  the tile rows falls by as much (`entropy_wall - entropy_sum`: 0.85 ms without, 0.23 ms with). The
  machine is oversubscribed in this run (the rasteriser's threads), and a tile that hops between
  threads that have just slept is placed on an idle core each time. This is not the console's
  situation, where the GPU is not on the CPU; the table above is the idle case.

CI, run 43468494277 on a throwaway branch (WARP, the runner's four processors shared with WARP;
`parser_timing.py`, three rounds, `entropy_sum` in milliseconds a frame and its ratio to `-parser c`):

| Stream | Threads | `-parser c` | `-parser rav1d` | `nolfmask` | `tilethread` | `stage` | `all` |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| 2160p 8-bit (`nf-dvbdash-a1-8b-2160p`) | 1 | 9.20 | 8.16, 0.89 | | | | 5.40, 0.59 |
| | 4 | 9.62 | 7.32, 0.76 | 4.74, 0.49 | 7.74, 0.80 | 7.28, 0.76 | 5.06, 0.53 |
| 1080p 10-bit (`nf-chimera-10b-1080p`) | 1 | 7.14 | 5.40, 0.76 | | | | 4.34, 0.61 |
| 1080p 10-bit (`nf-sparks-10b-1080p`) | 1 | 4.15 | 3.29, 0.79 | | | | 2.62, 0.63 |
| 1080p 10-bit, 8 tiles (`syn-tiles-10b-1080p`) | 4 | 6.70 | 4.02, 0.60 | | | | 3.86, 0.58 |

- With four threads on the eight tiles the wall time of the parse goes from 3.49 to 2.83 ms with the
  switches (0.83 to 0.68 of the C parser's 4.18).
- With `time`, the calls of the sink are 0.83 ms of the 2160p frame, the block lists 0.22 ms, the
  staged copies 0.02 ms.
- With more than one thread `entropy_wall` is 2.5 to 4.8 ms above `entropy_sum` on the 2160p stream on
  this runner, with and without the switches: a frame's parse starts on a worker thread, and a thread
  that wakes on a runner whose processors WARP keeps busy waits for one. The console measured 1.0 to
  1.3 ms there with `-t 6` and 0.04 ms with the C parser.

**Gates** (run 43468494277 on a throwaway branch, all 17 jobs green; the same on `main` with the
decode error check added): the parse check against the entropy decoder of libaom
with `-parseopt all`, one thread and four: 252 of 252 streams equal (with `stage` a zero inside a
transform block where libaom writes nothing is the one accepted difference); `rav1g-dec -parser rav1d
-parseopt all,time` on WARP: 252 of 252 md5, MSVC build and cross build; md5 of every frame and the
stage dumps of the seven dump streams against `-parser c`; the part of the set with `-poisonbuf`, 137
of 137; the player's decoder code with `-parser rav1d -parseopt all,time`, 4 of 4; the decode error check
(cut streams) with `-parseopt all`. Everything that existed is unchanged, with both parsers and no
switch. Locally the 244 AOM vectors under wine with `all,time` and with `stage`, and four cut streams,
whose errors and frames are the same with and without the switches.

**For the console.** The switches are off by default and the C parser stays the default: the console
decides. One launch, the same clip, two runs each:

```text
-parser c
-parser rav1d
-parser rav1d -t 1
-parser rav1d -parseopt tilethread
-parser rav1d -parseopt nolfmask
-parser rav1d -parseopt stage,time
-parser rav1d -parseopt time
-parser rav1d -parseopt all
-parser rav1d -parseopt all,time
```

- `-t 1` parses on the thread that sends the data, without rav1d's scheduler: on a clip with one tile
  it is what `tilethread` should reach with `-t 6`.
- `stage,time` against `time`: `entropy_sum - sink_lists - sink_calls` is rav1d with and without the
  stores into write-combined memory, and `sink_staged` is what the copies cost.
- `sink_lists` and `sink_calls` say what the sink is on the console.

**What follows from which answer.**

- `tilethread` and `nolfmask` are meant to become the way the parser works, and `nolfmask` then a
  removal in the copy of rav1d instead of a switch.
- If `stage` wins on the console, it stays and becomes the default; if not, it goes.
- If `entropy_wall - entropy_sum` stays near 1 ms with `tilethread`: a frame with one tile can be
  parsed on the thread that sends the data, also when there are worker threads (rav1d does that only
  when it has one thread). Not done.
- If `sink_lists` is large on the console: the lists of a superblock can be counted when the superblock
  ends, while its records are in the cache, in the same order. Not done; the C parser has the same
  pass.
- If the ratio is still far from the design's after that: `-parseopt time` gives rav1d by itself on the
  console, and the comparison to make is that number against the C parser's tile decode.

**Seen on the way.** A panic on a worker thread of rav1d ends nothing: the decode call waits for the
frame for ever (it happened here with an index one past an array of timers; step 4 has the item).
rav1d clears and makes anew its loop filter masks for every frame (700 KB at 4K) also with `nolfmask`,
because the CDEF indexes of a superblock live in them; that is frame set-up, outside `entropy_sum`.

### Console after step 3: rav1d is the default

2026-11-13. The console timed the switches of [Where the time goes](#where-the-time-goes), and the
answer decided the default.

**The console.** Package 0.2.138.0, `-t 6 -frate -1 -limit 300 -queue_size 4 -nopresent -gputime`,
frames per second, the mean of two runs:

| Variant | `j4k40` (heavy 4K 10-bit, 40 Mbps) | `t40x1ld` (SVT 4K 10-bit, 40 Mbps) | `a1_8b4k` (light 4K 8-bit) |
| --- | ---: | ---: | ---: |
| `-parser c` | 19.5 | 22.7 | 53.4 |
| `-parser rav1d` | 24.5 | 29.0 | 45.8 |
| + `tilethread` | 25.2 | 29.8 | 47.7 |
| + `nolfmask` | 24.6 | 30.3 | 58.6 |
| + `all` (`tilethread`, `nolfmask`, `stage`) | 25.2 | 31.2 | 59.4 |
| `-parser rav1d -t 1` | 25.1 | 29.6 | 49.0 |

- `nolfmask` is what the light clip needed (45.8 to 58.6 frames per second: faster than the C parser
  instead of slower), `tilethread` gives 0.7 to 1.9 frames per second on each clip, and the two
  together are the best row of every clip.
- `stage` gave nothing that could be measured: the direct stores into write-combined memory are not
  what the parse waits for on the console.
- The md5 set (15 clips) with `-parser rav1d -parseopt tilethread,nolfmask`: 15 of 15, no error.
- `j4k40` with `all`: 39.7 ms a frame, 29.0 ms of it the parse (`cpu parse`): rav1d by itself about
  20 ms, the calls of the sink about 3.4 ms, **the block lists 4.7 ms** (the largest part of the sink),
  and about 9 ms of other work on the decode thread (set-up, memsets, recording). The parse of
  `t40x1ld` with `all` is 21.6 ms a frame, that of `a1_8b4k` 8.3 ms.
- So heavy 4K is at 25 frames per second, not yet at 30: the block lists and the calls of the sink
  are the next thing to cut (below), then frames in parallel (step 4) and the other work of the
  decode thread.

**What changed.**

| Where | The parser of a decoder | Its switches |
| --- | --- | --- |
| Rust | `Settings::parser`: `Parser::Rav1d` (default) or `Parser::Libaom` | `Settings::parser_options`: `ParserOptions::default()` has `no_lf_masks` and `tile_keeps_thread` on; `ParserOptions::NONE` has neither |
| C (`rav1g.h`) | `Rav1gSettings::parser`: `RAV1G_PARSER_RAV1D` (what `rav1g_settings_default` writes, and what every value but `RAV1G_PARSER_LIBAOM` selects) or `RAV1G_PARSER_LIBAOM` | `Rav1gSettings::parser_options`: `RAV1G_PARSER_OPTIONS_DEFAULT` (what `rav1g_settings_default` writes: `_NO_LF_MASKS` and `_TILE_KEEPS_THREAD`); 0 for neither |
| `rav1g-dec`, test player | no flag or `-parser rav1d`; `-parser c` for the entropy decoder of libaom | no flag: `nolfmask,tilethread`; `-parseopt` says otherwise, per task ([test-player.md](test-player.md#switches-of-the-parser--parseopt)) |
| `rav1g-parse-check` (the oracle) | both, always | no flag: `nolfmask,tilethread`; `-parseopt` |

How a default switch is turned off, for an A/B on the device: a `-parseopt` list that names
`nolfmask`, `tilethread`, `none` or `default` replaces the default of those two, so the parse is done
in the ways the list names and in no other; `time` only adds the timing.

| `-parseopt` | `nolfmask` | `tilethread` | Sink calls timed |
| --- | --- | --- | --- |
| no flag, `default` | on | on | |
| `time` | on | on | yes |
| `none` | | | |
| `none,time` | | | yes |
| `tilethread` | | on | |
| `nolfmask` | on | | |

These are the rows of the table above: `-parseopt none` is its `-parser rav1d`, `-parseopt
tilethread` and `-parseopt nolfmask` its two single rows, and no flag is what the md5 set was run
with.

- The two stay switches of the copy of rav1d (`ParseSettings::lf_masks`, `::tile_keeps_thread`,
  upstream's behaviour unless set; `PATCHES.md`) and are set by `ParserOptions`. `nolfmask` was meant
  to become a removal in the copy; it stays a switch until the console no longer needs the A/B.
- **`stage` is gone**: the switch, its 32 KiB and 8 KiB of staging per tile in the sink, the timer
  `sink_staged`, the bit 1 of the options (it means nothing now; the other bits kept their values) and
  the one difference the oracle accepted for it (an explicit zero inside a transform block). The word
  `all` went with it: it would now name the default.
- `-parser c` stays, and with it everything of the C parser, until step 6.
- The parse digest (`rav1g-dec -parsedigest`, `rav1g-parse-check -digest`) parses with the default
  switches; the numbers are those of before, as they have to be.

**CI**, job by job: a check that names no parser is now a check of rav1d with its default switches.

| Job, check | Before | Now |
| --- | --- | --- |
| `rust`, `rust-cross-run`: the whole set, `-capi`, cut streams, order check, memory check, `-poisonmi`, `-poisonbuf`, `-interfast`, player smoke, timing smoke, stage dumps, MEMORY lines | the C parser | rav1d, `nolfmask,tilethread`; the conformance run has to say so in its `PARSER` line |
| the same checks "once more" | with `-parser rav1d` | with `-parser c` (the run has to say "the entropy decoder of libaom") |
| the two parsers against each other (md5 of every frame, stage dumps, MEMORY lines) | default against `-parser rav1d` | `-parser c` against the default |
| the switches | `-parser rav1d -parseopt all,time` | `-parser rav1d -parseopt none,time`: rav1d as upstream schedules it, which no other run has any more |
| `rust-equal` (the C++ decoder of branch `cpp`) | against the default run, its dumps and MEMORY lines | against the run, the dumps and the MEMORY lines of `-parser c`: the same parser on both sides, records byte for byte |
| cut streams (`cut_tile_check.py`) | `--parser rav1d` asked for rav1d's words | `--parser` defaults to rav1d; `--parser c` for libaom's words; the tasks always carry `-parser`, and the log has to name that parser |
| player smoke (`player_smoke.py`) | `Parser:` line checked with `-parser rav1d` | checked always: rav1d and `nolfmask tilethread` without a flag, libaom with `-parser c`, `none` with `-parseopt none` |
| `parse-check` | no switch; `-parseopt all` | the default switches; `-parseopt none`; one thread and four each |
| `parse-digest` | | unchanged: the same lines from three builds |
| paired timing (`parser_timing.py`) | `-parser c`, `-parser rav1d`, `all` | `-parser c`, `-parser rav1d` (default switches), `none`, `time`, and each switch alone on the 2160p stream |
| the packages of the test player | `-parser rav1d` on a task line | rav1d without a flag; import tables unchanged (rav1d was already linked) |

**The slow state of the console: the state every timing has to be taken in.** About 10 to 15 seconds
after the last controller input the console drops into a slower state, and stays there: the CPU is
about 25 % slower, the memory bandwidth of the CPU is 4 to 6 times lower, the GPU is not affected. No
interface lets an application hold the fast state, so playback runs in the slow one from its first
quarter of a minute to its end. A timing run that starts right after its launch is partly in the fast
state and says more than the device gives; a run for a number that counts has a warm-up task in
front of it, so that all of it is in the slow state. In that state memory traffic of the CPU is what
is expensive: bytes the CPU does not write or fetch count for more than instructions it does not
run. (The tables above were taken without this rule; their ratios stand, the numbers of single runs
may be high.)

**The clears of a slot leave the decode thread.** A second measurement on the console, the same
package and flags with rav1d and `-parseopt all`, all runs in the slow state, on `d2`: a 300-frame
cut, from 15 s, of the open stream `AV1/DVB-DASH/D_2.mp4` of the Netflix Open Content set (3840x2160,
10-bit HDR, 50 frames per second, about 46 Mbps, one tile a frame; 90 to 123 KB a frame, where
`j4k40` has 77 KB). Two runs each:

| | Clears on the decode thread | `-asyncclear` |
| --- | ---: | ---: |
| Frames per second out | 29.24, 29.29 | 33.54, 33.91 |
| Parse, mean | 24.5 ms a frame | 27.3 to 27.6 ms |
| Memsets on the decode thread | 7.5 to 7.6 ms a frame | none |
| `async_clear` on the completion thread | | 4.2 to 4.3 ms |

- 15 % more frames per second, and the md5 matched. The C parser on the same clip: 21.0 frames per
  second. (A first pair of runs had 38.5 frames per second for `-asyncclear`: that run was partly in
  the fast state.)
- The parse is about 3 ms slower with it: the clears now run next to the parse, on another thread,
  and take memory bandwidth from it. 7.5 MB cleared in 7.5 ms is 1 GB a second: the clears are bound
  by memory, not by the processor. What is not cleared at all costs neither thread anything
  ([below](#block-lists-and-clears-less-memory-traffic)).
- The GPU is busy about 30 % of the time on this clip, so a slot is back, cleared, long before the
  decode thread asks for one. When `-asyncclear` was first timed (2026-11-11, two slots, the GPU as
  the limit) the decode thread waited for the slot as long as it had cleared before, and the flag
  gave nothing ([test-player.md](test-player.md#pipeline-slots-and-clears--slots--gpuclear--cpuclear--syncclear)).
- **So the clears on the completion thread are the default** of the library
  (`rav1g::switches::set_async_clear`, `rav1g_set_async_clear`: on unless turned off), of `rav1g-dec`
  and of the test player. `-syncclear` puts them back on the decode thread, for the A/B and as the
  path of before; `-asyncclear` is still accepted and names the default. (Since package 0.2.146.0
  the default clears nothing, and the clears on the completion thread are `-cpuclear`:
  [No clears by the CPU](#no-clears-by-the-cpu).) In CI every run that does
  not say `-syncclear` now has the clears on the completion thread (the whole set with both parsers,
  the cut streams, the order check, the memory check, the stage dumps, which `rust-equal` holds
  against the C++ decoder, whose clears are on the decode thread); the other side has the whole set
  once more in both Windows-decoder jobs, the memory check once more, and a task of the player
  smoke.
- What `d2` says about what is left: at 50 frames per second it needs 20 ms a frame. Its mean parse
  is 24.5 ms, and every 24th frame is a large hidden frame whose parse takes up to about 195 ms, on
  one tile, which no tile thread can split. Frames in parallel (step 4) are the lever for it.

**The memory check** (`memory_check.py`). Its expected numbers do not change. What it holds is the
video memory after every release (0 bytes with both parsers), the growth of the private bytes of
the process from iteration to iteration, and the decisions of the budget guard; the device buffers of
a decoder do not depend on its parser (the MEMORY lines of the two are equal: 470.7 MiB for the
2160p 8-bit decoder, 166.9 MiB for 1080p 10-bit with `-lowmem`, 58.9 MiB for 720p 8-bit, as before).
What differs is host memory while a decoder exists: rav1d's parse state next to the C parser's, which
a decoder still allocates. Run 43475533217, the program of the cross build, eight iterations, private
bytes of the process in MiB (lowest to highest of the eight, and the mean):

| Task, when | `-parser c` | `-parser rav1d` |
| --- | ---: | ---: |
| 2160p 8-bit, decoded | 651 to 834, 798 | 659 to 848, 806 |
| 2160p 8-bit, released | 373 to 511, 461 | 371 to 520, 485 |
| decoder of the 4K 10-bit size, decoded | 835 to 1026, 949 | 842 to 1025, 940 |
| decoder of the 4K 10-bit size, released | 192 to 298, 279 | 195 to 306, 284 |
| 1080p 10-bit, three slots, decoded | 444 to 499, 486 | 452 to 504, 491 |
| 1080p 10-bit `-lowmem`, decoded | 448 to 505, 486 | 454 to 512, 492 |
| 720p 8-bit, decoded | 461 to 502, 485 | 476 to 509, 491 |

So 5 to 8 MiB more while a 1080p or 2160p decoder is in use, inside the run-to-run spread of WARP
(tolerances: 40 MiB of growth, 96 MiB after a release), and the lowest level after a release, which
is the number that repeats, is the same (192 against 195 MiB). The check now runs with rav1d by
default and once more with `-parser c`; the tolerances are as they were.

### Block lists and clears: less memory traffic

2026-11-13, behind the console's numbers above: of the 29.0 ms a frame of `j4k40` takes to parse, 4.7
ms were the block lists and 3.4 ms the other calls of the sink.

**What the block lists are.** For every block the host counts the prediction blocks it needs into
the lists the prediction stages run over (`av1_mi_push_block`, `crates/rav1g/src/host/blocks.rs`):
inter blocks by type and size, intra transform blocks by their wavefront iteration (one more than
the highest iteration of the neighbours they read, kept in a grid with an entry per 4x4 unit) and
size. The entropy decoder of libaom calls it for every record of a tile when the tile is parsed, and
the sink did the same: one pass over the records at the end of each tile.

**What made them 4.7 ms.** `j4k40` has 13,000 blocks a frame: 360 ns a block on the console, 105 ns
on the development machine (1.4 ms a frame). That is 3.4 times as long, like the other calls of the
sink (3.5 times), where rav1d by itself takes 1.6 times as long there: what the two have in common
is that they move memory, which is what the console is slow at
([the slow state](#console-after-step-3-rav1d-is-the-default)). Three things were found:

1. *The pass read a frame's records cold.* By the end of a tile the records (2.9 MB a frame of
   `j4k40`), their units of the grid and the neighbours the lists look at have left the cache. The
   pass fetched them again and wrote each record once more (its index into the list of block
   indexes), so every record went to memory twice.
2. *The lists of the wavefront iterations were cleared 5.2 MB at a time.* The setup of a tile clears
   the lists of its first 272 iterations. When a block reached iteration 257 the function cleared
   all the lists the tile can ever have: 5.2 MB for a 4K frame with one tile, of which the frame then
   uses a few thousand counters. That happens in 97 of 300 frames of `j4k40` (8 of `t40x1ld`, 3 of
   `a1_8b4k`, the key frame of the `d2` cut): 1.7 MB a frame on average, inside the timer of the block
   lists, and all of it in the frames that are heavy anyway.
3. *Inside the function the intra part is the larger one*: with it left out (an experiment, not a
   mode) the lists of `j4k40` take 0.67 instead of 1.48 ms on the development machine, those of
   `t40x1ld` 0.21 instead of 0.57. It reads and writes the wavefront grid along rows and columns for
   every transform block. Not changed here; see what is open.

**What changed.**

- **The lists are counted superblock by superblock**, when the superblock is parsed and its records
  and its part of the grid are in the cache (`Sink::count_lists`, from the sink's `superblock`). The
  function is called for the same records in the same order with the same block context, and it
  reads only what is parsed by then (the block's record, records above it and to its left through
  the grid, wavefront entries of blocks before it), so the lists are the same. `-parseopt latelists`
  (`ParserOptions::late_lists`, `RAV1G_PARSER_OPTION_LATE_LISTS`) counts them at the end of the
  tile as before, for the A/B.
- **The lists of the wavefront iterations are cleared as the wavefront gets there**: those of 1,024
  more iterations (40 KiB) when a block reaches an iteration whose lists are not cleared yet
  (`clear_iteration_lists` of `blocks.rs`). Everything that reads the lists reads those of the
  iterations a tile has reached and one counter more. With both parsers: the function is the host's.
- **The mode info grid of a slot is no longer cleared for a frame of the size of the frames before
  it on the slot** (`rav1g_setup_frame_begin`; a decoder with rav1d's parse). The parse writes the
  unit of every 4x4 of the frame, so what the frame before left inside the frame is overwritten
  before anything reads it: the sink reads a unit behind its block, the block lists read units of
  blocks that are parsed, the stages run when the parse is through, and a frame whose parse fails is
  not submitted. Outside the frame the grid has to be null, and is: it was cleared when the slot first
  had a frame of this width, height and row length, and only such frames were parsed into it since
  (the slot remembers them; a frame of another size clears as before). That is 4.2 MB of the 7.5 MB
  a 4K frame cleared, on whichever thread does the clears. `-parseopt cleargrid`
  (`ParserOptions::clear_grid`, `RAV1G_PARSER_OPTION_CLEAR_GRID`) clears for every frame as before.
- **The timer of the block lists** (`sink_lists`, "cpu parse: block lists") is taken with the
  processor's time stamp counter around the lists of a superblock, two readings a superblock, in
  every timing run with rav1d's parse; it means the same with `latelists`.
- **The other calls of the sink were looked at and left.** Their profile on the development machine
  (`j4k40`): 28 % of the call for a block is the stores of its pointer into the grid, 12 % the clear
  of its record, the rest the fields of the record. Both are the first writes into memory the stages
  read: the record's four cache lines and the grid's units have to be written once, and a record
  filled without the clear would touch the same lines. No bytes to save there in this layout.

Memory the CPU no longer moves, for a 4K frame:

| What | Before | Now |
| --- | --- | --- |
| Clears of a slot: the mode info grid and the wavefront grid | 7.5 MB | 3.5 MB (the wavefront grid) |
| Clear of the iteration lists, in a frame whose wavefront passes 256 iterations | 5.2 MB (`j4k40`: 1.7 MB a frame on average) | 40 KiB for every 1,024 iterations |
| Records fetched again for the lists, and written back a second time | all of the frame's (`j4k40`: 2.9 MB) | none |

**What is the same, and the one thing that is not.** Every buffer a stage reads holds the bytes it
held: the oracle (`rav1g-parse-check`: both parsers, every buffer over its whole size) is equal on
the conformance set and on the local 4K clips with the lists counted either way, one thread and
four; the stage dumps of the default, of `-parseopt latelists` and of `-parseopt none,latelists,time`
are equal byte for byte, mode info records included (CI holds the last against the default), and
equal to those of `-parser c` in what the two parsers have equal; the frames are those of the
references, and of the build before on the local 4K clips (`j4k40`, `fg4k_300`, `t12x1`,
`a1_8b4k_300`, 40 frames each). Not the same: the block map behind the lists of the iterations a
frame has reached now holds what an earlier frame counted there instead of zero. Nothing reads it,
and no stage dump has it.

**Measured on the development machine**, whose memory is fast, so that it shows the instructions and
little of the traffic (`rav1g-parse-check -time`, one thread, the block lists in ms a frame, two runs
each):

| Clip | Before | The list clears in steps, lists at the end of a tile (`latelists`) | Lists by superblock (default) |
| --- | ---: | ---: | ---: |
| `j4k40` | 1.377, 1.390 | 1.316, 1.317 | 1.250, 1.221 |
| `t40x1ld` | 0.512, 0.509 | 0.497, 0.497 | 0.482, 0.471 |
| `d2` (the cut of CI, 30 temporal units) | 0.947, 0.949 | 0.919, 0.917 | 0.849, 0.844 |

With another process streaming through memory next to the parse, the lists by superblock were 19 %
faster than at the end of a tile on `j4k40` (1.71 against 2.11 ms) where they are 5 to 7 % faster
without: the less cache and bandwidth the parse has, the more the order counts. Under wine the
clears of the `d2` cut are 3.5 MB a frame instead of 7.5 MB, on the decode thread (`-syncclear`) and
on the completion thread alike, and 7.5 MB again with `cleargrid`.

**Measured on CI** (run 43517057205 on `4d02423`, all 17 jobs green; before: run 43504802601 on
`ef044fa`). Two kinds of machine, and they do not say the same.

The Linux runner, parse and sink without a GPU (`rav1g-parse-check -time`, one thread, the best of
three; the block lists and, with `time`, the other calls of the sink, in ms a frame). Its caches hold
a frame and nothing competes for its memory:

| Stream | Lists before | Lists at the end of a tile (`latelists`) | Lists by superblock (default) | Other calls of the sink |
| --- | ---: | ---: | ---: | ---: |
| `nf-dvbdash-d2-10b-2160p` (new) | | 0.574, 0.541 | 0.583, 0.583 | 1.07, 1.06 |
| `nf-dvbdash-a1-8b-2160p` | 0.325 | 0.252, 0.260 | 0.304, 0.301 | 0.57, 0.55 |
| `nf-chimera-10b-1080p` | 0.225 | 0.224, 0.224 | 0.233, 0.232 | 0.28, 0.28 |

- The clear of the iteration lists in steps is the 0.325 to 0.256 ms of the 8-bit 4K stream (its key
  frame cleared 5.2 MB; 23 frames).
- Counting by superblock costs about 0.05 ms a 4K frame here, whatever the stream: the list code is
  entered 2,040 times a frame instead of once, cold each time, and with a frame's records still in
  the cache at the end of the tile it has nothing to win.

The Windows runner, the whole decoder on WARP, which rasterises on the same processors and competes
for cache and memory (`parser_timing.py`: the variants in turns on one runner, three rounds, one
parser thread; ms a frame, the mean and the range of the rounds' means):

| Stream | `sink_lists`, at the end of a tile (`latelists`) | `sink_lists`, by superblock (default) | `entropy_sum`, default | `entropy_sum`, `-parser c` |
| --- | ---: | ---: | ---: | ---: |
| `nf-dvbdash-d2-10b-2160p` | 1.183 (0.735 to 1.919) | 0.727 (0.709 to 0.755) | 20.19 | 36.85 |
| `nf-dvbdash-a1-8b-2160p` | 0.270 (0.257 to 0.281) | 0.250 (0.233 to 0.262) | 5.86 | 10.10 |
| `nf-chimera-10b-1080p` | 0.478 (0.404 to 0.515) | 0.316 (0.304 to 0.340) | 4.76 | 7.34 |
| `nf-sparks-10b-1080p` | 0.185 (0.155 to 0.223) | 0.151 (0.149 to 0.155) | 2.84 | 4.45 |
| `nf-dvbdash-d2-10b-2160p`, four threads, two rounds | 0.730 | 0.698 | 19.89 | 37.86 |
| `syn-tiles-10b-1080p` (8 tiles), four threads | 0.226 | 0.236 | 4.41 | 7.09 |

- Where the parse shares cache and memory with something else, the lists by superblock take 7 to 39 %
  less than the pass at the end of the tile, and vary less from round to round (the pass ranges from
  0.74 to 1.92 ms on the 10-bit 4K stream, the lists by superblock from 0.71 to 0.76). That is the
  console's situation more than the Linux runner's is, and its slow state more still.
- Against the run before (another runner, on which the calls of the sink, which did not change, are 5
  to 12 % slower): `sink_lists` 0.369 to 0.316 ms on `nf-chimera-10b-1080p`, 0.172 to 0.151 on
  `nf-sparks-10b-1080p`, 0.234 to 0.250 on the 8-bit 4K stream.
- With `time` the other calls of the sink are 1.56 ms a frame of the 10-bit 4K stream on this runner,
  1.14 ms of the 8-bit one.
- The 10-bit 4K stream: rav1d's parse takes 0.55 of the C parser's time (20.2 against 36.8 ms a
  frame), the widest gap of the set.
- What the stream costs CI: a decode of its 30 temporal units takes 11 s on WARP. The `rust` job
  went from 43.6 to 50.5 minutes (with it in seven runs of the whole set and 20 timed decodes),
  `parse-check` from 2.6 to 3.7 minutes. (The `rust` job was split and the timed decodes taken off
  the push on 2026-11-13: [warp-decoder.md](warp-decoder.md#the-jobs-of-a-push).)
- The memory check is as before with all of it: the video memory after every release is 0, the device
  buffers are those of before (497.1 MiB in use for the 2160p 8-bit decoder, 657.9 for the decoder of
  the 4K 10-bit size), the lowest private bytes after a release 195 MiB (193 before); with
  `-syncclear` and with `-parser c` the same.

**For the console**, all in the slow state, `-gputime`, the same clip in one launch:

```text
-parseopt latelists,cleargrid
-parseopt latelists
-parseopt cleargrid
(no -parseopt)
```

The first line is the package before this change in everything but the clear of the iteration lists.
To read: `cpu parse: block lists` (the lists; with `latelists` the pass at the end of the tile),
`cpu entropy/tile decode, thread sum` (the parse with everything in it), `cpu clears, completion
thread` and its MB a frame (7.5 with `cleargrid`, 3.5 without), and the frames per second.

**Console.** Package 0.2.141.0, the four lines above on `d2` and `j4k40`: slow state (the warm-up
task first), `-gputime`, 300 frames, two rounds. A cell has the value of round 1 and of round 2.

| | `latelists,cleargrid` (old) | `latelists` | `cleargrid` | default |
| --- | --- | --- | --- | --- |
| `d2`, frames per second | 33.33 / 33.46 | 33.74 / 34.07 | 34.64 / 34.70 | 34.95 / 34.63 |
| `j4k40`, frames per second | 28.16 / 28.18 | 28.53 / 28.63 | 28.86 / 28.83 | 28.93 / 28.96 |
| block lists, ms a frame, `j4k40` | 4.09 | 4.08 / 4.02 | 3.21 / 3.19 | 3.24 / 3.24 |
| block lists, ms a frame, `d2` | 1.67 | 1.61 / 1.62 | 1.18 / 1.17 | 1.18 / 1.20 |

- The clears on the completion thread: 7.5 MB a frame in about 4.2 ms with `cleargrid`, 3.5 MB in
  about 2.2 ms without.
- The decode thread: 33.3 ms a frame of `j4k40`, 27.0 ms of `d2`.
- The md5 set: 15 of 15 with the defaults.
- The defaults are the best of the four on the console. The lists by superblock win there (3.2
  against 4.1 ms a frame of `j4k40`, 1.2 against 1.6 of `d2`), which they do not on the Linux runner
  (0.05 ms a 4K frame more than the pass at the end of the tile). Both defaults stay.

**The 10-bit 4K stream of CI.** `nf-dvbdash-d2-10b-2160p.ivf`: 30 temporal units from the key frame
at 15 s of the open stream `d2` is a cut of (`testing/conformance/streams.txt`): the key frame (1.6
MB), two frames of 1.0 and 1.3 MB that are decoded and shown later, inter frames of 3 to 150 KB, and
every other temporal unit a `show_existing_frame`. It is in the conformance set (253 streams), in
the oracle, and it leads the paired timing of the parsers and the timing of the sink, in front of the
8-bit `a1` stream.

**Open after this.**

- *The wavefront grid* is what is left of the clears (3.5 MB a 4K frame, set to "no iteration") and
  of the list time (the intra part). `gen_pred_blocks.hlsl` reads one entry of it per intra
  transform block, the one at the block's bottom right; all the others are the host's own notes. A
  host that kept the iterations along the row above and the column to the left in two small arrays
  would write that one entry and clear nothing. What it has to get right: the corner above and to the
  left, which neither array holds; the luma under a chroma-from-luma block, which is read as an
  area; blocks that are not intra, whose place has to read as "no iteration". Clearing the grid only
  around the intra blocks of a frame is the smaller step, but its regions are shared by the threads
  of two tiles. Not done here; done in [No clears by the CPU](#no-clears-by-the-cpu).
- *The lists on a thread of their own*, behind the parse by a superblock row, would take them off
  the parse thread of a frame with one tile. Step 4 gives the parse of two frames two threads, lists
  included, so it comes first.
- *Frames as values (step 4)*: counting by superblock keeps its state in the tile (how many of its
  records are counted), as before; `av1_mi_push_block` still finds the frame through "the current
  frame" of the decoder (`curr_frame_data`: the wavefront grids, the warp flags of the references),
  and the kept grid is a property of the slot. Both have to become the frame's when frames are values.
- *The calls of the sink*: 3.4 ms a frame of `j4k40` on the console, first writes into the mode info
  pool and the grid (above). Fewer bytes there would mean another layout for the stages.

### No clears by the CPU

2026-11-13, behind the console's numbers of [Block lists and clears](#block-lists-and-clears-less-memory-traffic):
with the defaults of package 0.2.141.0 the completion thread still cleared 3.5 MB for every 4K frame,
in about 2.2 ms, all of it the wavefront grid; and when the clears first went to that thread the
parse of `d2` got 3 ms a frame slower (24.5 to 27.4 ms), because the clears and the parse share the
memory bandwidth the [slow state](#console-after-step-3-rav1d-is-the-default) leaves the processor.
The parse is what a frame waits for (33.3 ms of the decode thread for `j4k40`, 27.0 for `d2`), the
GPU is busy about 30 % of the time. So: no thread of the CPU clears anything of a slot for a frame.

**What was cleared.** Two grids of a pipeline slot, for every frame (`rav1g_setup_frame_begin`,
`crates/rav1g/src/host/frame.rs`): the mode info grid, a pointer per 4x4 unit, which has been kept
from frame to frame since the section above, and the wavefront grid of intra prediction
(`gen_intra_inter_grid`): an iteration number per 4x4 unit of luma and of chroma, set to "no
iteration" (-1). 3,673,300 bytes for a 3840x2160 frame.

**What the wavefront grid is for.** Intra prediction runs on the GPU as a wavefront: a transform block
is predicted in the iteration behind the highest one of the neighbours it reads, and the host
decides the iterations while it counts the block lists (`av1_mi_push_block`,
`crates/rav1g/src/host/blocks.rs`). It wrote the iteration of a transform block along the block's
right and bottom edge in the grid, and a later block read the units along the row above it and the
column to its left from there. The clear is what made every other unit read as "no iteration": the
units of inter and palette blocks, and units that are not decoded yet. The GPU reads the grid too,
but one entry per transform block (`gen_pred_blocks.hlsl`: the block's own iteration, to find its
list), an entry the host has just written.

**Removed: the wavefront at the edges of the tiles.** What a block reads of the grid is never far
from the border of what its tile has decoded: units in the row right above the block and in the
column right to its left. So a tile now keeps that border and nothing else (`WaveEdges`, a struct per
tile and plane):

| Kept | Entries | Holds |
| --- | --- | --- |
| `above`, `above_end` | one per column of the plane | the iteration at the lowest decoded unit of the column; the row below the last transform block of the wavefront there |
| `rows`: left | 32, one per row of a superblock row | the iteration at the rightmost decoded unit of the row |
| `rows`: corner | 32 | the iteration at the unit above that one: above and to the left of the block that comes next in the row |
| `left_end` | 32 | the column right of the last transform block of the wavefront in the row |

- Every block is entered there when its lists are counted: a transform block of an intra block with
  its iteration (`WavePlane::commit`), an inter or palette block with "no iteration"
  (`WavePlane::pass`). The blocks of a tile come in an order in which a block is behind every block
  above it and every block to its left, so the entry of a column is the unit right above the next
  block of that column, and the entry of a row the unit right to the left of the next block of that
  row.
- A transform block reads the corner, the row above over its width and the column to its left over
  its height, which are decoded whenever it may read them, and takes them from the three arrays
  (`wavefront_iteration_edges`). A block predicted from luma (CFL) also waits for the luma under it:
  that of the block itself, which was just counted, and for a block under 8x8 that of the blocks to
  its left and above it, which is at the edges before the block's own luma is entered.
- **Units further to the right and further down.** A block may also read above and to the right of
  it, and below and to the left, where `has_top_right` and `has_bottom_left` of libaom say that
  these units are decoded. For an inter-intra block the host asks them with the size of the block's
  residual transform and predicts the block whole, so they can name units that are not decoded yet;
  the cleared grid answered "no iteration" there. The edges answer the same: a transform block
  leaves a mark of where it ends in its columns and rows, and such a unit counts only if the mark
  says that the last transform block there ends right at the block that asks. The mark has the
  number of the frame in it (the frames of a slot are counted), so one an earlier frame left never
  passes for one of this frame, and one of an earlier superblock row or of a block further up names
  another row or column. A block without an iteration writes no mark: where a mark is asked about
  its units, the mark does not fit or the entry holds the block's "no iteration" by now.
  **So the edges are not cleared either**: an entry that is read was written by the frame that
  reads it.
- One more thing of the old function that the new one has to repeat to give the same lists: a
  chroma transform block outside the frame (the chroma loop has no clip at the frame's edge) has
  counts below zero and reads nothing, not even the corner.
- **Idle.** Most blocks of most frames are inter blocks with no intra block near them, and entering
  their "no iteration" is all the edges would cost them. So a tile keeps its edges only while it
  has transform blocks of the wavefront near (`WaveEdges::idle`): it starts idle, and it goes idle
  again at the start of a superblock row when the row before had none. A block reads units of its
  own superblock row and of the one above, so while the tile is idle every unit a block can read is
  "no iteration", and nothing is entered. The first intra block behind that wakes the tile: the
  edges are set to what they would hold by then, "no iteration" in every column of the tile and in
  every row (6 KB for a 4K frame with one tile, once per stretch of the frame that has intra
  blocks), and are kept from there on.
- Of the grid the host now writes the one entry per transform block that the shader reads. The
  grid is not cleared: what the shader reads of it, the frame has written.
- Memory: the rows above are 12 bytes a column (4 KB of iterations and 8 KB of marks for the luma
  of a 4K frame, half of that for chroma, per row of tiles: 17,856 bytes), the rest 1,072 bytes a
  tile. They belong to the slot, grow to what its largest frame needs and are allocated from the
  heap of the process (`WaveStore`), not from the host pool: the buffers of a decoder and the MEMORY
  lines are those of before. A frame for which the process has no memory for them keeps the
  wavefront in the grid.

**Moved: the clear by the GPU (`-gpuclear`).** The other way to take a clear off the CPU, built as a
switch: the wavefront stays in the grid, and the last commands of the frame's command list clear the
grids of its slot (`record_slot_clears`): `fill_buffer.hlsl`, the shader that clears the residuals,
over the bytes the frame's setup cleared, behind the output copy, which is the last thing of a frame
that reads anything of the slot. (The output pass itself is untouched: copy and film grain are one
pass as before.) The command list is through before the frame's fence signals, the slot goes back to
its pool on that fence, so the next frame of the slot finds the grids as its own clear would leave
them. The rule about sizes of the clears on the completion thread holds unchanged: what is cleared
behind a frame is what the frame cleared, and a frame of another size clears for itself.

- *The heap.* The grids are `HostRW` buffers: a custom heap in the memory pool L0 with the page
  property write-back, in the state `GENERIC_READ`. D3D12 fixes the state of a buffer only in an
  upload or a readback heap; a buffer of a custom heap can be in any state, if it was created with
  the flag for it. The coefficient buffer has been such a buffer from the start (a custom heap,
  write-combined, written by the CPU and zeroed by the IDCT shaders). For `-gpuclear` the two grids
  are created with `ALLOW_UNORDERED_ACCESS`, and the clear moves them to `UNORDERED_ACCESS` and
  back. Only a decoder that is created while the mode is set has such grids, so **a decoder without
  the flag creates nothing a package before did not create**; in a decoder created without it the
  mode is the clears on the completion thread.
- *Does the CPU see what the GPU wrote?* The host reads and writes the grids through the mapping
  it has had since the creation, made with an empty read range. On an adapter with one memory for
  both and coherent caches (`CacheCoherentUMA`: WARP, and what the console reports) that is the
  same memory and the caches agree, so it does; Bale measured the other direction there (the GPU
  reading what the CPU wrote through a cached mapping). On an adapter without it the mapping would
  have to be made again with the range to read. **Not checked on the console**: a run with
  `-gpuclear -md5_frame_check` says it, since a grid the CPU sees stale gives wrong iterations.
- With `-gpuclear` the mode info grid is cleared by the GPU as well where it is cleared at all
  (`-parser c`, `-parseopt cleargrid`).

**The switch.** One setting with four values; the three that are not the default keep the wavefront
in the grid, which is the path of every package so far:

| | Rust (`rav1g::switches::set_clears`) | C (`rav1g_set_clears`) | `rav1g-dec`, test player |
| --- | --- | --- | --- |
| The wavefront at the edges of the tiles, nothing cleared (default) | `Clears::NotNeeded` | `RAV1G_CLEARS_NOT_NEEDED` | no flag |
| The grid, cleared by the frame's command list | `Clears::Gpu` | `RAV1G_CLEARS_GPU` | `-gpuclear` |
| The grid, cleared by the completion thread (default of 0.2.140 to 0.2.144) | `Clears::CompletionThread` | `RAV1G_CLEARS_COMPLETION_THREAD` | `-cpuclear` (`-asyncclear`) |
| The grid, cleared by the decode thread (default up to 0.2.139) | `Clears::DecodeThread` | `RAV1G_CLEARS_DECODE_THREAD` | `-syncclear` |

It is a switch of the process like the one before it (`av1_set_clears` of `dx/av1_perf.h`), read by
every frame at its setup and set while no frame is in flight; `RAV1G_CLEARS_GPU` has to be set
before the decoder is created. `set_async_clear` / `rav1g_set_async_clear` stay as its older form
(true: the default; false: the decode thread). The test player creates a decoder of its own for a
task with `-gpuclear`.

**What each cleared buffer does now**, for a 3840x2160 frame with one tile:

| Buffer | Up to 0.2.144, default | Now, default | `-gpuclear` | `-cpuclear` | `-syncclear` |
| --- | --- | --- | --- | --- | --- |
| Wavefront grid, 3,673,300 bytes | cleared by the completion thread | **not cleared, not needed** | cleared by the GPU | completion thread | decode thread |
| Mode info grid, 4,177,920 bytes, rav1d's parse | kept, not cleared | kept, not cleared | kept | kept | kept |
| Mode info grid with `-parser c` or `-parseopt cleargrid` | completion thread | completion thread | cleared by the GPU | completion thread | decode thread |
| The edges of the wavefront (new) | | not cleared | | | |
| Counters of a tile's block lists and the tile's descriptor, 12,368 bytes a tile | tile thread | tile thread | tile thread | tile thread | tile thread |
| Lists of the wavefront iterations past the first 272, 40 KiB per 1,024 iterations | tile thread | tile thread | tile thread | tile thread | tile thread |
| Coefficients | zeroed by the IDCT shaders as they are read | the same | | | |

So without a flag the CPU clears 12,368 bytes a tile and frame (272 of the tile's descriptor and
12,096 of counters, which its tile thread counts the block lists in right after), and for a frame
whose wavefront is long 40 KiB more per 1,024 iterations: 0.3 % of the 3.5 MB. These counters are
the tile thread's own working memory, read and written by it as soon as they are cleared, so a
clear by the GPU would only make the thread fetch them; they are left where they are. They do not
shrink with the tile: a 1080p frame with eight tiles has 98,944 bytes of them.

**What is the same.** The iteration of every transform block, and with it every list.

- The oracle (`rav1g-parse-check`) has both ways (`-wave edges`, the default; `-wave grid`) and a
  mode that holds one against the other: `-wave check` keeps the wavefront in the grid for the
  entropy decoder of libaom and at the edges for rav1d. The block lists and the block maps are then
  equal only if every transform block of the frame gets the same iteration either way, and every
  entry of the grid the edges' side wrote has to be the grid's. The edges start as a pattern that no
  iteration has, so an entry that is read without having been written in its frame shows as a
  block in the tile's last iteration. Equal on the conformance set, with one thread and with four,
  with the lists counted at the end of a tile, on the local synthetic streams (lossless,
  monochrome, screen content, 128x128 superblocks with tiles, reference scaling, superres) and on
  300 frames each of `j4k40` (4.29 million transform blocks) and `t40x1ld` (1.63 million). That is
  where the special cases above were found: the inter-intra blocks in the first inter frame of the
  first vector, the chroma blocks outside the frame in a lossless stream.
- Under wine (the decoder on lavapipe, the 244 vectors of AOM): the frames are those of the
  references with the default, with `-gpuclear`, `-cpuclear` and `-syncclear`; with `-parser c` and
  `-parser c -gpuclear`; with `-poisonbuf` and `-poisonbuf -gpuclear -parseopt cleargrid`; with four
  threads, and with four threads, three slots and `-gpuclear`; through `rav1g.h` with and without
  `-gpuclear`. CI has the same on WARP, on the whole set.
- The stage dumps of `-cpuclear` (the grid) are equal to those of the default (the edges) byte for
  byte, the lists of the prediction blocks among them; CI holds them against each other
  (`rust-parsers-equal`).

**`-poisonbuf` and `-poisonmi`.** The grids start as 0xA5 bytes with `-poisonbuf`, as before. With
the default nothing clears the wavefront grid behind that, so an entry the shader reads without the
host having written it is a number no list has, not a "no iteration" a clear left there: the flag
now tests more of this grid than it did. The edges are filled with the byte again for every frame
(like the constants of the stages, whose memory is also used again from frame to frame): an entry
that is read unwritten is then "no iteration" where the block there has one, which is a block
predicted too early, and a mark that names no decoded unit. With `-gpuclear` the fill of the
creation is what the first frame of a slot clears on the decode thread, as with the other two
flags; from then on the grid holds what the GPU's clear left, and a clear that did not reach a byte
the next frame reads would show as that frame's iterations. The mode info pool of `-poisonmi` is
not touched by any of this.

**Measured on the development machine** (`rav1g-parse-check -time`, one thread, the block lists in ms a
frame, two runs each; the machine's memory is fast, so this is instructions, not traffic):

| Clip | The wavefront in the grid (`-wave grid`) | At the edges (default) |
| --- | ---: | ---: |
| `j4k40` | 1.424, 1.423, 1.446 | 1.466, 1.505, 1.472 |
| `t40x1ld` | 0.554, 0.547, 0.537 | 0.486, 0.505, 0.499 |
| `d2` (the cut of CI, 30 temporal units) | 0.908, 0.921, 0.903 | 1.017, 1.021, 1.021 |

- A transform block costs less at the edges than in the grid (`t40x1ld`). What costs is that a
  block without an iteration enters its border while its tile is not idle, where with the grid it
  did nothing: about 12 ns a block here, and `d2` has 13,000 blocks a frame, nearly all of them
  inter blocks, with an intra block somewhere in most superblock rows (without the idle state its
  lists are 1.07 to 1.10 ms). So where memory is fast the lists are 0.04 to 0.11 ms a frame slower
  on two of the three clips and 0.05 faster on the third, and the clear they replace is small too
  (0.17 ms for the 3.5 MB under wine on this machine).
- The console is the other way round: the clear is 2.2 ms a frame there and takes bandwidth from
  the parse, and the lists by the grid read and write memory that is not in the cache, which the
  edges do not. Its `cpu parse: block lists` line and its frames per second decide; `-gpuclear`
  is the way without the border writes, if they should cost more there than the grid does.
- A variant in which a block without an iteration writes only its corners, and every read is held
  against a mark instead, is as exact and slower where a frame has many intra blocks (`j4k40` 1.66
  to 1.71 ms, `t40x1ld` 0.58 to 0.60): not taken.

**Measured under wine** (the decoder on lavapipe; bytes cleared for a frame of the 30 temporal unit
cut of `d2`, from the frame lines of the timing run, `testing/conformance/clears_check.py`):

| Way | CPU, decode and tile threads | CPU, completion thread | GPU |
| --- | ---: | ---: | ---: |
| no flag | 12,368 | 0 | 0 |
| `-gpuclear` | 12,368 | 0 | 3,673,300 |
| `-cpuclear` (the default of 0.2.144) | 12,368 | 3,673,300 | 0 |
| `-syncclear` | 3,685,668 | 0 | 0 |
| `-parseopt cleargrid` | 12,368 | 4,177,920 | 0 |
| `-gpuclear -parseopt cleargrid` | 12,368 | 0 | 7,851,220 |
| `-parser c` | 12,368 | 4,177,920 | 0 |
| `-gpuclear -parser c` | 12,368 | 0 | 7,851,220 |

These are the frames from the third on. The first frame of each of the two slots clears on the
decode thread, whatever the way (first use: 4.2 MB of the mode info grid without a flag, 7.9 MB
of both grids with one of the three). The clear of 3.5 MB is a stage of 0.92 ms on lavapipe, a
software rasteriser (1.0 % of what the stages of a frame take there); what it is on a GPU is the
console's `gpu slot_clear` line.

**CI.** A job of its own, `rust-clears`, next to `rust-switches` (which keeps the `-syncclear` run of
the whole set):
`clears_check.py` on the 2160p stream and on the stream with eight tiles (the table above as a
check: without a flag no byte on the completion thread or the GPU and less than 64 KiB a tile on
the tile threads; with `-gpuclear` every frame's command list clears, with the stage in it;
`-cpuclear` clears the same bytes on the completion thread; `-syncclear` on the decode thread;
both grids with `-parser c` and `-parseopt cleargrid`); the whole set and the memory check with
`-gpuclear`; the whole set and the stage dumps with `-cpuclear`; a
part of the set with `-gpuclear -parser c` and with `-gpuclear -poisonbuf -parseopt cleargrid -slots
3 -t 4`. `rust-cross-run` decodes the whole set with `-gpuclear` with the program of the Linux
build, the player smoke test has a task with each flag, and `parse-check` runs `-wave check` with
one thread and with four and `-wave grid` once.

Run 43558835990 on `310b510`, all jobs green, 16.4 minutes for the push (`rust-clears` 7.9 minutes;
run 43556652825 on `6ecd347` before it, the same without the idle state, 16.4 minutes):

- **Who clears what**, the 2160p stream on WARP: the bytes of the table above, frame by frame
  (12,368 on the tile threads without a flag and nothing else; 3,673,300 on the GPU with
  `-gpuclear`, in every frame's command list; the same on the completion thread with `-cpuclear`;
  7,851,220 on the GPU with `-gpuclear -parseopt cleargrid` and with `-gpuclear -parser c`). The
  stream with eight tiles: 98,944 bytes on the tile threads without a flag, 1,226,100 on the GPU
  with `-gpuclear`.
- **What the clear takes** on that runner, whose memory is fast: the stage `slot_clear` 0.40 ms a
  frame for the 3.5 MB (0.2 % of the stages of a frame on WARP, which rasterises on the CPU) and
  1.08 ms for 7.9 MB; the completion thread 0.09 ms for the 3.5 MB, the decode thread 0.09 ms.
- **The lists either way**: `rav1g-parse-check -wave check` equal on 254 of 254 streams with one
  thread and with four (2.0 million transform blocks), `-wave grid` equal; the stage dumps of
  `-cpuclear` equal to those of the default byte for byte; the dumps, the frames and the MEMORY
  lines of `-parser c` equal to those of the C++ decoder as before (`rust-equal`).
- **The lists' time on the Linux runner** (`rav1g-parse-check -time`, one thread, the best of three,
  two runs; ms a frame), the grid and then the edges: `nf-dvbdash-d2-10b-2160p` 0.861, 0.844 and
  0.961, 0.956; `nf-dvbdash-a1-8b-2160p` 0.448, 0.445 and 0.517, 0.503; `nf-chimera-10b-1080p`
  0.320, 0.316 and 0.409, 0.409. As on the development machine: where memory is fast and nothing
  competes for it, the edges cost 0.06 to 0.11 ms a frame of instructions more than the grid. On
  WARP, with four threads and the rasteriser next to the parse, `cpu parse: block lists` of the
  2160p stream is 0.66 ms without a flag, 0.58 with `-gpuclear`, 0.67 with `-cpuclear` and 0.60
  with `-syncclear`: single runs, which do not tell the ways apart.
- **The memory check with `-gpuclear`**: 40 decoders, every frame as its reference; the video memory
  after a release 0 bytes above the start; the floor of the private bytes at most 13.3 MiB above
  that of the fourth iteration (tolerance 40); the device buffers those of before (470.7 MiB for
  the 2160p 8-bit decoder, 166.9 MiB for 1080p 10-bit with `-lowmem`, 58.9 MiB for 720p 8-bit).

**The packages.** 0.2.148.0 (and `.2`, MSVC) of run 43558835990 is the one this section describes.
0.2.146.0 and 0.2.147.0 (runs 43554948108 and 43556652825) have the same without the idle state;
in the first of the two runs a check of CI still asked the log for the old default and failed,
with every decode passing.

**For the console**, all in the slow state (the warm-up task first), `-gputime`, the same clip in
one launch:

```text
-i /streams/<clip>.ivf -t 6 -frate -1 -limit 300 -queue_size 4 -progress -nopresent -gputime -cpuclear
-i /streams/<clip>.ivf -t 6 -frate -1 -limit 300 -queue_size 4 -progress -nopresent -gputime
-i /streams/<clip>.ivf -t 6 -frate -1 -limit 300 -queue_size 4 -progress -nopresent -gputime -gpuclear
-i /streams/<clip>.ivf -t 6 -frate -1 -limit 300 -queue_size 4 -progress -nopresent -gputime -syncclear
```

The first line is package 0.2.144.0. To read: `cleared per frame, bytes` (3.5 MB on the completion
thread in the first line, on the GPU in the third, on the decode thread in the fourth; a few KB on
the tile threads in the second), `cpu clears, completion thread`, `gpu clears, end of the command
list` and the stage `gpu slot_clear` (what the clear costs the GPU), `cpu parse: block lists` (the
lists, where the edges are), `cpu entropy/tile decode, thread sum` (the parse with everything in
it, which the clears of the first line take bandwidth from), `cpu decode thread, setup to submit`,
and the frames per second. The md5 of every frame with each, in launches of their own
([test-player.md](test-player.md#same-output-md5-of-every-frame-3)): the line without a flag and
the line with `-gpuclear` are the two the console has not run, and `-gpuclear` is the one that
says whether its processor sees what its GPU wrote into the grids.

**For step 4** (frames in parallel, frames as values, slots per frame).

- The edges are the frame's: memory of its slot, a stamp per frame of the slot, pointers a tile
  gets at its setup. Two frames in parse on two slots share nothing of it. When a frame becomes a
  value, `WaveStore` and the stamp go with the slot it holds, as the kept mode info grid does.
- The default has no state between two frames of a slot for the wavefront any more: no "clean"
  mark, no size to match. The marks stay for the three flags and for the mode info grid of
  `-parser c`, until step 6 removes that parser.
- The clear by the GPU is ordered by the frame's own fence and touches only the frame's slot, so
  it holds with frames in parallel as it is. It is the path to take if step 4 or 5 brings a buffer
  that does need a clear per frame: `record_slot_clears` is a list of (buffer, bytes, value).
- The switch is read from a static of the process at the setup of a frame
  (`frame::clears_mode`), like the shader switches; `av1_mi_push_block` still finds the frame
  through "the current frame" of the decoder, as noted above.

**Open.**

- The console: the four lines above, and the md5 set with the default and with `-gpuclear`.
- The counters of a tile's block lists (12,368 bytes a tile and frame) and the lists of long
  wavefronts are what the CPU still clears. Both are counters the tile thread uses at once.
- The intra part of the block lists now works in the cache; what it costs on the console is
  the `cpu parse: block lists` line of the run above.

### Colour description and HDR metadata

2026-11-13. Not a step of the plan: the interface reported the size and the bit depth of a stream
and nothing about its colour, and an application that shows the frames needs it
([color.md](color.md)). Both parsers now pass on the colour description of the sequence header and
the two HDR metadata OBUs, and the output is the bytes it was.

- **rav1d** had all of it: `parse_seq_hdr` reads `color_config`, and the metadata OBUs of the
  types `HDR_CLL` and `HDR_MDCV` are kept in the context and attached to every picture, which the
  cut of step 1 left alone. What the sink was not given is now given
  (`third_party/rav1d/PATCHES.md`): the three code points of a sequence header as numbers
  (`ParseColorCodes`; the header has them as enums in which a reserved value is "unspecified"), and
  with a frame and with a shown slot the HDR metadata upstream gives the picture
  (`ParseHdrMetadata`). 27 lines more in the diff against upstream; nothing of the reading changes.
- **The host** (`rav1d_host.rs`) writes the description into the sequence parameters of its
  `AV1_COMMON` and the metadata into a new member of it, where the C parser has them too; the
  output image of a frame takes both from there (`copy_output`), and the completion thread hands
  them to the application with the frame. No stage reads any of it.
- **The C parser** read the two OBUs and dropped the values; it keeps them now, with
  `color_description_present_flag`, and forgets them at a sequence header that differs from the one
  before, which is rav1d's rule.
- **Which frames an OBU goes with** is rav1d's rule (`color.md`, "Which frames the HDR metadata goes
  with"): from the OBU on, until another of its kind or a new sequence; a `show_existing_frame` has
  what holds when it is shown.
- **For step 4** (frames in parallel): the description and the metadata of a frame are read from
  the `AV1_COMMON` of "the frame in parse" when its output image is taken, like everything else of
  the frame header. They become the frame's with the rest.

CI: job `colour` holds what `rav1g-dec` prints for every stream against FFmpeg's `trace_headers`
and `ffprobe` with libdav1d, with both parsers and through both interfaces; the set has an HDR10
stream for it (`syn-hdr10-10b-1080p.ivf`, the 254th).

## Licence

rav1d is BSD-2-Clause (copyright VideoLAN and the dav1d authors, and the rav1d contributors); the
assembly files carry their own notices. The vendored copy keeps `COPYING` and the file headers, and the
repository's licence text and README name it when step 1 lands. The AOMedia Patent License 1.0 notice
stays: it covers the AV1 implementation, and the shaders and the host remain derived from AOM code after
the C parser is gone. libaom's own files and the libwebm notices follow those files.

Since step 1: `third_party/rav1d/COPYING` and `doc/PATENTS` are in the tree and in the `licenses`
directory of every CI artifact with a program (the test player's packages, `rav1g-dec` of both builds);
the README names rav1d. The crates rav1d depends on that end up in the library (`parking_lot`,
`bitflags`, `libc`, `strum`, `zerocopy`, `atomig`, `raw-cpuid`, `av-data`, `cfg-if`, `paste`,
`static_assertions`, `to_method`, `assert_matches`) are MIT or Apache-2.0, some either. Since step 3
their notices, and those of the Windows bindings, are collected in
`third_party/rust-crates/NOTICES.txt` and are in the `licenses` directory of every package.

## The experiment patch

Not in the repository; five small changes to rav1d at the pinned commit, enough to repeat the numbers:

- a timer around `rav1d_decode_tile_sbrow` in `rav1d_worker_task`, summed per pass and, for pass 1, per
  frame context; a frame's sum is recorded in `rav1d_decode_frame_exit`;
- a switch that makes `rav1d_decode_tile_sbrow` return at once for pass 2 (with `--inloopfilters none`
  and film grain off the frame then costs its parse and the scheduler);
- a line per frame in `rav1d_submit_frame`: type, shown, `primary_ref_frame`, the decode index of the
  frame that supplies the CDFs, `refresh_context`, `disable_cdf_update`, tiles, `use_ref_frame_mvs`;
- counters in the first pass of `decode_b` (blocks, inter, skipped) and in `decode_coefs` (coded and
  empty transform blocks, coefficients up to the last, the size rav1d stores);
- a switch for `check_tile`: the entropy task waits for a reference only for temporal motion vectors or
  the previous segmentation map.

The schedule of [the CDF chain](#what-the-streams-allow-the-cdf-chain) is a script over those lines and
the frame sizes of the IVF file.
