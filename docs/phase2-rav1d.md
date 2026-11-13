# Phase 2: rav1d's parser in place of the C parser: design

Status: design, written 2026-11-12, with measurements on a development machine; steps 1 to 3 of the
[plan](#plan) are in the tree ([Progress](#progress)): the decoder runs on either parser, chosen when it is
created. Phase 1 ([rust-port.md](rust-port.md)) moved the host to Rust and kept the libaom-derived C parser;
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
both.

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

**Licence notices.** `third_party/rust-crates/NOTICES.txt` has the MIT and Apache-2.0 notices of the 32
crates of crates.io that are linked into the library and `rav1g-dec` (rav1d's dependencies and the
Windows bindings), written by `collect.py` there from `Cargo.lock`; CI holds the file against the lock
file and copies it into the `licenses` directory of every package.

**The test player** logs its lifecycle events, one `LIFECYCLE ...` line each
([test-player.md](test-player.md#inputs-and-logs-in-localstate)).

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
