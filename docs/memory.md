# Memory: what the streams use (`-memstats`)

2026-11-15. A measurement, to choose reductions from: the decoder for 3840x2160 10-bit takes 646.5 MiB
of device buffers in an application with 1 GiB ([phase2-rav1d.md, step 5](phase2-rav1d.md#step-5-the-pools)),
and most of them are sized for a worst case. Nothing of the decoder was changed for the measurement;
what was changed since is under [Reductions made](#reductions-made).

- [The flag](#the-flag)
- [The lines](#the-lines)
- [What was measured](#what-was-measured)
- [Where the time of the decode call goes](#where-the-time-of-the-decode-call-goes)
- [Candidates, ranked](#candidates-ranked)
- [Reductions made](#reductions-made)

## The flag

`-memstats` in a task line of the test player and of `rav1g-dec`; off by default. The decoder counts
while it decodes and logs a block of lines that start with `MEMSTATS ` when the task has drained
(`grep MEMSTATS`; the player's log puts its own prefix in front, `rav1g-dec` prints them as they are).
With `-progress` there is also a `MEMSTATS frame` line for every frame, in decode order.

```text
-i j4k40.ivf -limit 300 -nopresent -memstats
rav1g-dec -hw -i j4k40.ivf -limit 60 -memstats -progress
```

It is a process switch like the others (`rav1g_set_memory_stats(level)` of `rav1g.h`,
`switches::set_memory_stats`: 0 off, 1 the block, 2 also the frame lines; code in
`crates/rav1g/src/host/mem_stats.rs`). The frames are the same with it (the 244 vectors of AOM, with
`-memstats -progress`: 244 of 244). Off, every hook is one load of an atomic: one a tile, one a palette
block and four a frame. It can go together with `-gputime`.

## The lines

All counts are of the frames between two drains: a task. "Of a frame" is over the frames that were
recorded; "at once" is over all frames in flight together (in parse, or on the GPU), looked at when a
tile ends.

| Line | What it says |
| --- | --- |
| `begin` | Frames recorded, the largest frame, the decoder (size, bit depth, slots, frames in parse). |
| `pool mode_info_pool`, `pool idct_coefs`, `pool idct_blocks_unordered` | The three pools: chunks and size; **chunks in use at once, peak** (what a smaller pool has to hold without a wait); chunks one frame holds when it is recorded, peak and mean; the records (coefficients) one frame wrote, peak, as a share of the pool, and mean; the same at once; **fill**: records written over the room of the chunks held (what the chunk granularity costs); how often a tile **waited for a chunk of that pool**, for the GPU to finish an older frame and for a frame in parse in front of its own (the `MEMORY pools` line of a release has the sum of the three pools). A pool below the worst case of a frame says its share (`1695 chunks of 7680 coefficients (1632/2040 of the worst case of a frame)`), and its percentages are of the pool as it is. Coefficients are counted as the transform blocks take them, with the padding of their counts. |
| `buffer gen_mi_block_indexes` | The block index list of a slot (CPU, at the parse; since the [reduction](#stage-buffers-sized-for-the-frames-candidate-3) into lists of the tiles, copied into the slot's buffer at the recording): entries of a frame against the worst case. The lines of the four buffers that grow also say what the buffer has now and how often it grew. |
| `buffer prediction_blocks`, `buffer prediction_blocks_warp` | The records `gen_pred_blocks` writes on the GPU for a frame against the buffer. The counts are known on the CPU before the frame is recorded. |
| `buffer idct_blocks` | The sorted transform blocks (GPU): as many as the frame's transform block records. |
| `frame buffers` | The buffers of the pool that are out of it (a reference count above 0: named by a reference slot, the frame of a slot in flight, a reference such a frame still reads) at the setup of every frame, that frame's included: peak, the most frequent count, mean, and how many frames saw each count; and how often a frame found the pool empty and waited for a buffer. In a decoder whose frames take their buffer when they are recorded ([candidate 5](#the-frame-buffer-bound-at-the-recording-candidate-5)) the count is taken there: `out of the pool when a frame takes its buffer at its recording`. |
| `references` | How many different frames the 8 reference slots name at the setup of a frame, the same way. |
| `palette` | Palette buffers created; frames that allow screen content tools (each binds a palette buffer of the size of a frame buffer); frames that have palette blocks; the bytes of the palette blocks of a frame (what a palette buffer really holds), peak and mean, and at once. |
| `cpu bytes written` | CPU writes per frame into memory the GPU reads, mean and peak for all writes and for each part. It counts the mode info records (96 bytes each, down from 224 in the measurements below), the grid (4 bytes per 4x4 unit, down from 8), coefficients (an upper bound of 4 bytes per value up to the last nonzero one), transform block records (16 bytes), the block index list, CDEF lists, stage constants and palette blocks. This line leaves out the block-list counters, loop restoration units, film grain offsets and clears. The clears are reported by `-gputime`. |

Not measured, because the stream does not change them: `idct_residuals` (24.4 MiB) and
`loopfilter_blocks` follow the size of the frame; the grids and maps of a slot are addressed by position.

## What was measured

Locally: `rav1g-dec -hw` under wine on vkd3d and lavapipe (a software GPU: the frames stay in flight
longer than on a console, which the frame buffer counts and the "at once" numbers feel; the per-frame
numbers are the stream's). Defaults: at 4K two frames in parse on three slots. 60 temporal units of each
4K clip, the 30 of the `d2` cut, 120 of `j1080`.

**The pools, 2040 chunks (coefficients 2103).** Chunks in use at once, peak, as a share of the pool and
in MiB; behind it the fill of the chunks held.

| Stream | Mode info, 111.6 MiB | Coefficients, 61.6 MiB | Transform block records, 12.0 MiB |
| --- | --- | --- | --- |
| `j4k40` | 9.2 %, 10.3 MiB, fill 99 % | 9.9 %, 6.1 MiB, 99 % | 9.6 %, 1.1 MiB, 99 % |
| `a1_8b4k_300` | 10.7 %, 11.9 MiB, 96 % | 22.3 %, 13.7 MiB, 95 % | 20.0 %, 2.4 MiB, 93 % |
| `fg4k_300` | 7.9 %, 8.9 MiB, 28 % | 2.1 %, 1.3 MiB, 67 % | 6.8 %, 0.8 MiB, 71 % |
| `t40x8` (8 tiles) | 8.5 %, 9.5 MiB, 72 % | 26.3 %, 16.2 MiB, 96 % | 16.1 %, 1.9 MiB, 76 % |
| `t40x1ld` | 4.6 %, 5.1 MiB, 95 % | 17.3 %, 10.6 MiB, 99 % | 8.4 %, 1.0 MiB, 96 % |
| `d2` cut | **22.3 %**, 24.9 MiB, 99 % | **62.4 %**, 38.4 MiB, 99 % | **41.8 %**, 5.0 MiB, 99 % |
| `j1080` (pools of 540) | 8.9 %, 2.6 MiB, 97 % | 10.6 %, 1.7 MiB, 97 % | 16.9 %, 0.5 MiB, 95 % |

One frame at most: `d2` 263 chunks of mode info (12.9 %), 672 of coefficients (32 %), 533 of records
(26 %); `j4k40` 77, 70, 66. The low fill of `fg4k_300` and `t40x8` is the last chunk of a tile's piece in
frames that have next to nothing (8 and 16 chunks a frame on average): it costs no memory that matters.
The conformance vectors (352x288, a pool of 36 chunks) are the other end: the lossless and low quantizer
streams fill a frame's 58 % of the mode info pool, 74 % of the coefficients, 94 % of the records, and
with two frames in flight 78 %, 100 % and 100 %.

**The buffers of the stages.** A frame's peak as a share of the buffer:

| Stream | `gen_mi_block_indexes`, 6.0 MiB a slot | `prediction_blocks`, 23.9 MiB | `prediction_blocks_warp`, 9.0 MiB | `idct_blocks`, 12.0 MiB |
| --- | ---: | ---: | ---: | ---: |
| `j4k40` | 2.5 % | 3.8 % | 0.0 % | 3.2 % |
| `a1_8b4k_300` | 5.9 % | 7.6 % | 1.3 % | 10.6 % |
| `fg4k_300` | 1.4 % | 1.8 % | 0.0 % | 0.6 % |
| `t40x8` | 3.2 % | 4.6 % | 0.4 % | 5.9 % |
| `t40x1ld` | 2.3 % | 3.3 % | 0.0 % | 2.1 % |
| `d2` cut | 11.5 % | 15.8 % | 3.6 % | 26.1 % |
| `j1080` | 4.2 % | 5.9 % | 0.0 % | 4.9 % |
| conformance vectors, the most | 39.1 % | 55.5 % | 13.8 % | 93.8 % |

**Frame buffers** (11 in the pool and 2 back buffers at 4K, 24.2 MiB each at 10 bits) and the frames
the reference slots name:

| Stream | Out of the pool: peak, typical, mean | Reference slots: peak, typical, mean |
| --- | --- | --- |
| `j4k40` | 9, 8, 7.7 | 3, 2, 2.1 |
| `a1_8b4k_300` | 11, 11, 9.7 | 5, 5, 4.6 |
| `fg4k_300` | 7, 7, 6.7 | 4, 4, 3.8 |
| `t40x8` | 11, 11, 9.2 | 4, 4, 3.7 |
| `t40x1ld` | 5, 4, 4.4 | 1, 1, 1.0 |
| `d2` cut | 11, 9, 8.5 | 5, 5, 4.3 |
| `j1080` (10 in the pool) | 8, 8, 7.4 | 3, 2, 2.2 |
| conformance vectors, the most | 9 | 7 |

No stream here has more than 5 different frames in its reference slots (7 in a conformance vector), yet
three of the six 4K runs empty the pool: with two frames in parse on three slots, up to three frames
are in flight with their own buffer and the references they read, which the reference slots have
dropped by then. That part depends on how far the GPU is behind, so the console will show less; the
reference counts are the stream's.

**Palette.** None of the 4K clips and not `j1080` has a frame that allows screen content tools: no
palette buffer is created (0 of the 72.5 MiB). Three conformance vectors have palette blocks; the most
a frame writes is 36,032 bytes into a buffer of 3.2 MiB (1.1 %), 66,496 bytes at once.

**CPU bytes a frame**, MiB, mean / peak, and the mean of the larger parts:

| Stream | All | Mode info records | Grid | Coefficients, at most | CDEF lists |
| --- | --- | ---: | ---: | ---: | ---: |
| `j4k40` | 8.59 / 11.04 | 2.45 | 3.96 | 1.34 | 0.49 |
| `a1_8b4k_300` | 5.33 / 19.98 | 0.85 | 3.96 | 0.38 | 0 |
| `fg4k_300` | 4.12 / 6.26 | 0.13 | 3.96 | 0.01 | 0.01 |
| `t40x8` | 8.45 / 13.45 | 0.63 | 3.96 | 3.16 | 0.50 |
| `t40x1ld` | 7.63 / 10.65 | 0.73 | 3.96 | 2.38 | 0.38 |
| `d2` cut | 9.99 / 41.84 | 2.79 | 3.96 | 2.79 | 0 |
| `j1080` | 2.71 / 3.69 | 0.91 | 0.99 | 0.50 | 0.13 |

The largest single part of every 4K frame is the mode info grid: a pointer of 8 bytes for each of the
518,400 4x4 units, 4,147,200 bytes, whatever the frame holds. The shaders read its low half only
(`cb_mi_addr_base`, `get_mi_index`). Transform block records, the block index list and the constants are
0.1 to 0.2 MiB together.

**Console (2026-11-15, package 0.2.175.0, slow state, Bale stopped, 300 temporal units of each 4K clip,
2 frames in parse on 3 slots).** "At once" is the peak over all frames in flight; "frame" the peak of one
frame.

| Stream | Mode info at once / frame | Coefficients at once / frame | Tx records at once / frame | `prediction_blocks` frame | `idct_blocks` frame | Frame buffers out, peak / typical | Distinct refs, peak | CPU MiB per frame, mean / peak | App usage, 30 frames out |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| `j4k40` | 11.1 % / 5.3 % | 10.6 % / 4.1 % | 10.5 % / 3.8 % | 5.3 % | 3.8 % | 10 / 9 | 3 | 9.41 / 13.08 | 866.9 MiB |
| `d2_300` | 20.0 % / 12.8 % | 57.6 % / 33.3 % | 38.7 % / 26.1 % | 19.8 % | 26.1 % | 11 / 11 | 5 | 8.74 / 41.84 | 873.2 MiB |
| `a1_8b4k_300` (8-bit) | 10.7 % / 5.3 % | 22.3 % / 13.9 % | 20.0 % / 11.0 % | 8.6 % | 11.0 % | 11 / 11 | 5 | 5.20 / 20.13 | 859.6 MiB |
| `fg4k_300` | 8.5 % / 2.3 % | 29.5 % / 17.1 % | 17.7 % / 3.6 % | 3.6 % | 3.6 % | 11 / 7 | 7 | 5.66 / 17.98 | 674.3 MiB |
| `t40x1ld` | 4.6 % / 3.0 % | 16.4 % / 7.9 % | 8.7 % / 2.4 % | 3.3 % | 2.4 % | 5 / 4 | 1 | 7.91 / 11.65 | 870.3 MiB |
| `t12x1` | 6.7 % / 3.6 % | 15.0 % / 6.3 % | 10.7 % / 4.4 % | 4.0 % | 4.4 % | 11 / 11 | 4 | 6.14 / 11.42 | 854.4 MiB |

- No clip allows the screen content tools: no palette buffer was created on any of them.
- The pools: the mode info pool is used to 20 % at most (22.3 MiB of 111.6), one frame to 12.8 %; the
  coefficient pool to 57.6 % (`d2`), one frame to 33.3 %. So the mode info pool has the room, the
  coefficient pool little.
- The frame buffers are not oversized on the console either: four of the six clips have all 11 out at
  once, with no more than 7 distinct references; the rest are frames in flight.
- The mode info grid (4.15 MiB, fixed) is 42 to 80 % of what the CPU writes for a frame.

## Where the time of the decode call goes

No timer was added: `-gputime` has the split (`GPUTIME` summary, [test-player.md](test-player.md)).
`cpu entropy/tile decode, wall` and `thread sum` are the parse with the sink; `cpu parse: block lists`
is the block lists inside it, and with `-parseopt time` `cpu parse: sink calls` is the other writes of
the sink, so `thread sum less the above` is the entropy decoder with the coefficients it writes; `cpu
command list record` and `cpu submit` are the submission; `cpu memsets` and `cleared per frame` the
clears; `decode call` the call. `rav1g-parse-check -time` times the parse without a device. Numbers
that mean something come from the console in its slow state
([phase2-rav1d.md](phase2-rav1d.md#where-the-time-goes)); a `-memstats -gputime` run there gives bytes and
milliseconds of the same frames.

## Candidates, ranked

For the 3840x2160 10-bit default (646.5 MiB: frame buffers 314.2, pools 185.2, stage buffers 73.3,
slots 74.1). By MiB, with the risk; the numbers above are local and from seven streams.

| # | Reduction | Saves | What the numbers say | Risk |
| --- | --- | ---: | --- | --- |
| 1 | Frame buffers of 10 bits as three samples in 32 bits | 104.7 MiB (13 x 8.1), and a third of every palette buffer | Not a matter of the stream. `fbt10x3` is not this: it is a format of the **output image** (`tryHDR10x3`: the output copy and the film grain shader pack three samples into a word of the application's image). No stage reads or writes a packed frame buffer. | High. Every shader that touches a frame buffer (intra, inter, reconstruction, loop filter, CDEF, loop restoration, superres, borders, film grain, output) gets another addressing, and three samples share a word, so two threads of a dispatch that write neighbours write the same word: the writes have to be laid out by word. All of it in `cs_5_0`. Byte-identical is possible, the work is the largest here. |
| 2 | **Done at half, the default above 1920x1088**, [below](#the-wait-of-the-second-frame-in-parse-in-a-pool-at-half-candidate-2). Mode info pool at half, or a quarter, of the worst case | 55.8 or 83.7 MiB | At once at most 22.3 % (`d2`), 9 to 11 % elsewhere; one frame at most 12.9 %. The wait for the GPU exists and is tested (`poolwait`); a pool at half exists as `-lowmemparts 2`. | Low at half, medium at a quarter. A frame that alone does not fit is a decode error (`MEMORY FAILED`): at half an average block below 8x4 over a whole frame, at a quarter below 8x8; the low quantizer vectors reach 58 % at 352x288. A fallback would be a second buffer created on that frame, under the budget guard. More waits cost speed, not frames. |
| 3 | **Done**, [below](#stage-buffers-sized-for-the-frames-candidate-3) (the index list without a pool). Stage buffers at a quarter, grown on the frame that needs more: `prediction_blocks`, `prediction_blocks_warp`, `idct_blocks`, and the block index list as a fourth pool | 33.7 MiB (0.75 x 44.9) and 13.5 MiB (0.75 x 18.0) | A frame's peak is 15.8 %, 3.6 %, 26.1 % and 11.5 % (`d2`), a few percent elsewhere. | Medium. The three GPU buffers are used by one command list at a time and their counts are known before the frame is recorded, so a frame that needs more can get a larger buffer first (the guard may refuse: then a decode error). The index list is written at the parse: a pool with a piece for a whole block ([step 5, "Left"](phase2-rav1d.md#step-5-the-pools)). |
| 4 | **Done**, [below](#the-mode-info-record-field-by-field-candidate-4). Mode info record of 96 bytes | 31.9 MiB (of the 55.8 the pool has since 2) | The sink clears and fills 96 bytes a block where it used 224. | The shared layout changed for the sink, the block lists and the shader readers. The stage dumps hold the lists they produce equal. |
| 5 | **Done as part 32 of the low memory parts, in the default above 1920x1088**, [below](#the-frame-buffer-bound-at-the-recording-candidate-5). The frame buffer bound at the recording | 24.2 MiB (11 to 10 in the pool) | The reference slots name at most 5 frames in these streams; the pool is emptied by the frames in flight. | Medium: the references of a frame in parse are resolved at the recording, through every error path of step 4. The floor stays 10 (8 references, a failed frame, the next frame): the measured 5 do not allow less, a stream may use 8. |
| 6 | **Done as the part that exists (4, four fifths), in the default above 1920x1088**, [below](#the-coefficient-pool-at-four-fifths-candidate-6). Coefficient pool at three quarters | 15.0 MiB at three quarters, 12.0 at four fifths (the pool has 63 chunks for the ends of pieces that stay) | 62.4 % at once in `d2`, 10 to 26 % elsewhere. | Low, and little: `d2` would be near its waits, and half is not possible for it. A frame that needs more than the pool is a decode error. |
| 7 | Palette blocks from a pool instead of a buffer of the size of a frame a slot | 0 MiB for film (no clip allows screen content tools); 72.5 MiB less on the first frame of a stream that does | 36 KB a frame at most where a buffer is 3.2 MiB. | Medium: the shaders address the palette buffer like the frame buffer. It removes a growth of 72.5 MiB that the budget guard can only refuse. |

For the CPU (second priority, DRAM bandwidth in the slow state; the first of these is **done**,
[below](#the-mode-info-grid-in-four-bytes-a-unit)): the mode info grid as 4 bytes a unit
(an index, which is what the shaders use) halves the largest part, 2.0 of 8.6 MiB a frame on `j4k40`, and
takes 2.0 MiB off every slot; the host's readers of the grid (block lists, CDEF lists) then add a base.
The CDEF skip list (0.5 MiB a frame, record thread, read back through the grid) could be a GPU pass.
The smaller mode info record (4) reduces the other large part.

## Reductions made

### The mode info record, field by field (candidate 4)

The mode info record is 96 bytes. The default 3840x2160 10-bit decoder (two frames in parse, three
slots) takes **469.6 MiB** of device buffers, down from **501.5 MiB**. The local `MEMORY decoder created`
lines on Wine and lavapipe report 492,441,856 bytes after and 525,865,216 before, with the decoder size
rounded to 3840x2176. The mode info pool has 1020 chunks of 256 records. It shrinks from 55.8 to
23.9 MiB, saving 31.9 MiB (33,423,360 bytes). The pool at sizes up to 1920x1088 shrinks by the same
four sevenths.

The sink clears and fills 96 bytes a block where it used 224. For a busy 4K frame that is about
1.1 to 1.2 MiB of records, down from 2.5 to 2.8 MiB.

On the console (package 0.2.214 against 0.2.209, both measured after a restart of the console on
2026-11-18, three runs of 300 frames each, the mean): the md5 set of 15 streams is unchanged, and every
variant is faster, by less than one run differs from the next but in the same direction in all eight:

| Stream, options | fps 224 → 96 bytes | Entropy/tile thread sum, ms | Sink calls, ms |
| --- | --- | --- | --- |
| `j4k40`, default | 49.2 → 50.2 | 27.7 → 26.5 | |
| `j4k40`, `gpugrid` | 50.4 → 50.6 | 25.3 → 24.8 | |
| `j4k40`, `time` | 48.9 → 49.5 | 28.2 → 27.1 | 2.91 → 2.30 |
| `j4k40`, `time,gpugrid` | 50.0 → 50.3 | 26.0 → 25.3 | 1.34 → 1.21 |
| `d2_300`, default | 52.7 → 53.9 | 25.4 → 24.4 | |
| `d2_300`, `gpugrid` | 54.1 → 55.6 | 22.9 → 21.7 | |
| `d2_300`, `time` | 52.0 → 53.4 | 25.9 → 24.7 | 2.73 → 2.04 |
| `d2_300`, `time,gpugrid` | 53.6 → 55.0 | 23.1 → 22.1 | 1.23 → 1.00 |

The GPU's busy time is the same (18.4 and 9.8 ms a frame): the shaders that read the record do not
notice the narrower fields. The sink is 0.6 to 0.7 ms a frame cheaper, the parse threads about 1 ms.

The table shows what each of the 224 bytes of a record used to hold
and where the fields are used (`MB_MODE_INFO` of `crates/rav1g-sys/src/records.rs` and
`libav1/dx/shaders/mode_info.h`). The record is written by the sink (`crates/rav1g-parse/src/record.rs`, a cleared record filled for every
block; `cdef_strength` at the end of the superblock) and by the host (`index_base`, in
`av1_mi_push_block` of `crates/rav1g/src/host/blocks.rs`). The prediction and loop filter list shaders
read a `StructuredBuffer<MB_MODE_INFO>`, and `cdef_lists` and `mi_grid_build` read by byte offset.
The host reads the records in `blocks.rs` for the block lists,
`postfilter.rs` for the CDEF lists without the GPU pass, and `grid_check.rs`. The sink reads the
block's place and size for the grid and the host's call.

| Field | Old offset, bytes | Written | Read | In the 96-byte record |
| --- | --- | --- | --- | --- |
| `palette_colors[24]` | 0, 48 | sink, palette blocks | nobody: the colours reach the GPU with the block's index map (the sink's palette call, `write_palette`) | gone |
| `palette_size[2]` | 48, 4 | sink, intra blocks | `gen_pred_blocks`, `blocks.rs` (zero or not) | 2 (0 to 8) |
| `wm_params.wmmat[0..6]` | 52, 24 | sink, a local warp with a model | `gen_pred_blocks` | 24 |
| `wm_params.wmmat[6..8]` | 76, 8 | never (the model has six numbers) | nobody | gone |
| `wm_params.alpha` to `delta` | 84, 8 | sink, as `wmmat` | `gen_pred_blocks` | 8 |
| `wm_params.wmtype` | 92, 1 (and a byte of padding) | sink | nobody | gone |
| `wm_params.invalid` | 94, 2 | sink | `gen_pred_blocks` (one bit), `blocks.rs` | a bit |
| `interinter_comp.type` | 96, 4 | sink, inter blocks | `gen_pred_blocks`, `blocks.rs` | 1 (0 to 3) |
| `interinter_comp.mask_type` | 100, 4 | sink | `gen_pred_blocks` | a bit |
| `interinter_comp.wedge_index` | 104, 4 | sink | `gen_pred_blocks` | 1 (0 to 15) |
| `interinter_comp.wedge_sign` | 108, 4 | sink | `gen_pred_blocks` | a bit |
| `filter_intra_mode`, `use_filter_intra` | 112, 2 (and two bytes of padding) | sink | `gen_pred_blocks`, `blocks.rs` (the second) | 1 and a bit |
| `mv[2]` | 116, 8 | sink | `gen_pred_blocks`, `blocks.rs` | 8 |
| `interp_filters` | 124, 4 | sink | `gen_pred_blocks`, `blocks.rs` | 2 (two filters of 0 to 4) |
| `interintra_wedge_index` | 128, 4 | sink | `gen_pred_blocks` | 1 (0 to 15) |
| `interintra_wedge_sign` | 132, 4 | never (0 in AV1) | `gen_pred_blocks` adds it | gone |
| `current_qindex` | 136, 4 | sink, every block | nobody | gone |
| `delta_lf_from_base` | 140, 4 | sink | `gen_lf_blocks` | 1 (-63 to 63) |
| `delta_lf[4]` | 144, 16 | sink | `gen_lf_blocks` | 4 |
| `mi_row`, `mi_col` | 160, 8 | sink | `gen_pred_blocks`, `mi_grid_build`, `blocks.rs`, sink | 4 (a frame has at most 16384 units a side) |
| `index_base` | 168, 4 | host | `gen_pred_blocks` | 4 |
| `num_proj_ref` | 172, 4 | never | nobody | gone |
| `cfl_alpha_idx`, `cfl_alpha_signs` | 176, 8 | sink | `gen_pred_blocks` | 2 (0 to 255, 0 to 7) |
| `comp_group_idx`, `compound_idx` | 184, 4 | sink | nobody | gone |
| `sb_type`, `partition`, `ref_frame[2]` | 188, 4 | sink | all five shaders (`sb_type`, `ref_frame`), `blocks.rs` (`partition` too), sink | 4 |
| `mode`, `uv_mode`, `interintra_mode`, `motion_mode` | 192, 4 | sink | `gen_pred_blocks`, `gen_lf_blocks` (`mode`), `blocks.rs` | 4 |
| `use_wedge_interintra`, `use_intrabc`, `angle_delta[2]` | 196, 4 | sink | `gen_pred_blocks`, `gen_lf_blocks` (`use_intrabc`), `blocks.rs` | 4 |
| `tx_size`, `skip`, `segment_id` | 200, 201, 203 | sink | `gen_pred_blocks`, `gen_lf_blocks`, `cdef_lists` (`skip`), `blocks.rs`, `postfilter.rs` | 3 |
| `skip_mode` | 202, 1 | sink | nobody | gone (its byte is free) |
| `inter_tx_size[16]` | 204, 16 | sink, inter blocks | `gen_lf_blocks` | 16 |
| `cdef_strength` | 221, 1 | sink, at the superblock | `cdef_lists`, `postfilter.rs` | 1 |
| `seg_id_predicted`, `ref_mv_idx`, `reserved` | 220, 222, 223 | sink (not the last) | nobody | gone |

`tools/rav1g-parse-check` holds every field equal between the ways of running the parser and the parse
digest hashes the bytes of the record, both within one commit; the stage dumps `mi_grid` and `mi_pool`
show the fields by name (`MI_FIELDS` of `testing/stage_dump/stage_dump.py`).

- **76 bytes have no reader**: the palette colours (48), `wmmat[6..8]` (8), `wmtype` and three bytes of
  padding, `current_qindex`, `num_proj_ref`, the two compound indexes (4 each), `skip_mode`,
  `seg_id_predicted`, `ref_mv_idx`, `reserved`. `interintra_wedge_sign` is read and always 0.
- **The 148 bytes that are read fit 96**: every number of 32 bits but the model of the warp, the vectors
  and `index_base` has a range of a byte or less by the syntax of AV1, not by what a stream happens to
  use. No field is lost that a stage reads, and none is narrowed below its range.
- **What stays as it is**: the six numbers and four angles of a local warp (32 bytes; the largest part,
  and only a `WARPED_CAUSAL` block has them), the two motion vectors, `index_base`, the four words of
  byte fields (`block_type`, `modes`, `intra_mode_flags`, `tx_info`) and the transform sizes of an
  inter block (`inter_tx_size`, 16 bytes, the tree of the block one level under its largest
  transform).

The smallest layout without a second buffer, **96 bytes**, in words as the shaders load them:

| Offset | Bytes | Fields |
| ---: | ---: | --- |
| 0 | 24 | `wmmat[0..6]` |
| 24 | 8 | `alpha`, `beta`, `gamma`, `delta` (16 bits each) |
| 32 | 8 | `mv[2]` |
| 40 | 4 | `mi_row`, `mi_col` (16 bits each, unsigned) |
| 44 | 4 | `index_base` |
| 48 | 4 | `sb_type`, `partition`, `ref_frame[2]` |
| 52 | 4 | `mode`, `uv_mode`, `interintra_mode`, `motion_mode` |
| 56 | 4 | `use_wedge_interintra`, `use_intrabc`, `angle_delta[2]` |
| 60 | 4 | `tx_size`, `skip`, `cdef_strength` (where `skip_mode` was), `segment_id` |
| 64 | 16 | `inter_tx_size[16]` |
| 80 | 4 | `delta_lf[4]` (signed bytes) |
| 84 | 4 | `delta_lf_from_base` (signed), `cfl_alpha_idx`, `cfl_alpha_signs`, `interintra_wedge_index` |
| 88 | 4 | `palette_size[2]` (a byte each), `filter_intra_mode`, flags: bit 0 `use_filter_intra`, bit 1 the warp model is not valid, bit 2 `mask_type`, bit 3 `wedge_sign` |
| 92 | 4 | the compound type, `wedge_index`, the vertical and horizontal interpolation filters |

The four words at 48 to 63 use the old byte positions, with `cdef_strength` in the free byte of
`tx_info`. `gen_pred_blocks` uses the six coefficients and four signed shear parameters of the
local warp. The global warp constants still use their 44-byte `WarpedMotionParams` layout. The GPU
grid builder reads position as one aligned word, and the CDEF list shader reads both fields from
`tx_info`.

Before the change, three sets of three local runs of `rav1g-parse-check -time -threads 1` on busy 4K
streams put the sink at 1.1 to 2.0 ms and 1.1 to 3.6 ms per frame. Those numbers are differences of
runs with and without the sink.

**The equality reference.** The `rust-equal` job uses `shaders.py same --candidate-changed` for
`gen_pred_blocks`, `gen_lf_blocks_vert` and `gen_lf_blocks_hor`. The check requires these shaders to
differ from the C++ reference and the other shared shaders to match. The legacy
`gen_pred_blocks_fast` is built for comparison with C++. Rust uses `gen_pred_blocks_fast_packed`
for `-interfast`, listed under `--candidate-only` with `cdef_lists` and `mi_grid_build`. The stage dump
and per-frame md5 checks still hold the prediction, warp, loop filter and CDEF lists and the later
stages against the C++ decoder. `stage_dump.py` reads the 224- and 96-byte layouts by the dump's
`record=` and compares the shared fields of `mi_grid` and `mi_pool` through the grid. With
`--two-parsers` those differences are listed without deciding equality. Digests of mode info hold
CRCs of the original records, so a digest can compare units only within one layout.

### Stage buffers sized for the frames (candidate 3)

2026-11-15. `idct_blocks`, `prediction_blocks`, `prediction_blocks_warp` and the block index list of
every slot are created with a quarter of their worst case and made larger before the frame that writes
more is recorded (`crates/rav1g/src/host/stage_buffers.rs`). The decoder for 3840x2160 10-bit (two frames
in parse, three slots) takes **599.4 MiB** of device buffers where it took 646.5: 47.1 MiB less (33.7
for the three buffers of the stages, 13.5 for the index lists; a slot is 20.2 MiB where it was 24.7).

| Buffer | Worst case, MiB | Created with, MiB |
| --- | ---: | ---: |
| `idct_blocks` | 12.0 | 3.0 |
| `prediction_blocks` | 23.9 | 6.0 |
| `prediction_blocks_warp` | 9.0 | 2.2 |
| `gen_mi_block_indexes`, a slot | 6.0 | 1.5 |

- *When.* The counts are those of the tiles' lists when the parse of the frame has ended: the transform
  block records, and the lists `gen_pred_blocks` turns into offsets (`prediction::pred_block_counts`). The
  thread that records the frames looks at them before it records anything of the frame
  (`stage_buffers::for_frame`). A buffer that holds the frame's records and 64 more is left alone.
- *The index list.* The parse wrote a tile's entries into the slot's buffer at a place laid out for
  the worst case of the tiles in front of it, so the buffer could not be smaller. Each tile now writes a
  list of its own in host memory, which grows with the tile and is kept for the next frame
  (`TilePieces::indexes`; `av1_mi_push_block` makes room for a block before it writes), and the
  recording copies the lists into the slot's buffer one behind the other and tells `gen_pred_blocks`
  where each starts (`mi_idx_base`, as before). That is one copy of what the frame wrote (0.69 MiB for
  the largest frame of the `d2` cut, 0.14 MiB on average) by the thread that records, and as much host
  memory for each frame in parse; no shader changed. It was not made a fourth pool: a pool's pieces
  would have changed what a block's `index_base` means and with it the mode info the reference is
  compared with.
- *How.* The thread waits until the queue has run every command list that was submitted (a fence behind
  them: the frames in flight read the old buffer), releases the buffer and creates the new one in the
  same descriptor, so the two never exist together. The new size is an eighth above what the frame
  needs, in steps of 64 KiB, and at most the worst case, which is what the buffer always was before.
  The index list of a slot is read by the frame of that slot only, so it grows without the wait.
- *It sticks.* A buffer never gets smaller: a stream that had one such frame will have more, and a wait
  for the queue on every other frame would cost more than the memory is worth. A decoder that has decoded
  the worst case holds what it held before this change.
- *The guard.* A growth is held against the application's budget as a palette buffer is: what it adds
  and the margin of 16 MiB (`AV1_MEMORY_GUARD_MARGIN_BUFFER`) have to fit what the callback returns, or
  the frame is a decode error (`AOM_CODEC_MEM_ERROR`, a `MEMORY REFUSED buffer ... grown for frame n`
  line), nothing is allocated and the device is intact.
  It does not wait: the memory an application has left does not come back by waiting, and the frame
  cannot be recorded without the buffer. The headroom of the creation does not refuse a growth, as it
  refuses no creation. An allocation that fails (`MEMORY FAILED buffer ..., grown for a frame`) is the
  same decode error; the buffer is then gone and the next frame creates it again. The most they
  can add, 47.1 MiB at 4K with three slots, is in `device_size_deferred` of the memory query, next to the palette
  buffers, and in the `MEMORY budget` line.
- *The lines.* `MEMORY decoder created stage buffers: idct_blocks 3.0 of 12.0 MiB, ...,
  gen_mi_block_indexes 1.5 of 6.0 MiB a slot` at the creation; `MEMORY buffer idct_blocks grown for
  frame 0, which writes 204595 records: 3.0 to 3.6 MiB of the 12.0 MiB of the worst case; device buffers
  now 600.0 MiB` at a growth. `-memstats` keeps its percentages
  against the worst case and adds what the buffer has now and how often it grew: `buffer idct_blocks:
  12.0 MiB at worst, 3.6 MiB now, grown 1 times, ...`.
- *What grows.* Of the 4K clips only the `d2` cut: its first frame writes 204,595 transform blocks
  (26.1 %), `idct_blocks` goes from 3.0 to 3.6 MiB once. The conformance vectors (352x288) grow a buffer
  48 times in the 244 runs (4 of them an index list), up to the worst case.
- *Checked locally* (wine on lavapipe): the 244 vectors of AOM with the library's defaults and with two
  frames in parse on two threads, 244 of 244 each; the `d2` cut 30 of 30. A
  growth that the guard refuses was not produced: the budget of `rav1g-dec -budget` is one number for
  the creation and for the growth.
- **Console** (2026-11-15, package 0.2.184.0, with the four byte grid below): md5 15 of 15 with
  `-memstats`. No growth was refused; the 4K clips grew nothing, `b8_720_300` and `b8_1080_300` one
  buffer once each. The numbers of both reductions together are at the end of the next section.

### The mode info grid in four bytes a unit

2026-11-15. The grid of a slot has four bytes for every 4x4 unit where it had eight: the CPU writes
2,073,600 bytes of grid for a 3840x2160 frame where it wrote 4,147,200 (`cpu bytes written ... mode
info grid` of `-memstats`), a slot's grid is 2.0 MiB where it was 4.0, and the decoder for 3840x2160
10-bit (two frames in parse, three slots) takes **593.4 MiB** of device buffers (599.4 before; a slot
18.2 MiB).

- *The entry.* libaom's grid has the address of the block's record; the shaders read its low half,
  take the low half of the pool's address off (`cb_mi_addr_base`) and divide by the size of a record.
  The entry is now that one word, and not an address: where the record starts in the pool, in bytes,
  plus 224 (`rav1g_sys::MiGridEntry`, `mi_grid_entry`, `mi_grid_record`), with 224 as the base the
  shaders take off. A unit without a block is 0, which no record has. The arithmetic of the shaders is
  what it was, and nothing depends on where the pool is mapped.
- *The shaders.* `gen_pred_blocks.hlsl` (and `gen_pred_blocks_fast.hlsl`, which includes it) and
  `gen_lf_blocks_hor/vert.hlsl` read the size of an entry from a constant that was reserved
  (`cb_mi_grid_entry`: 4, or the 8 of an address). Nothing
  in them is beyond `cs_5_0`; FXC and DXC compile them.
- *The host.* The sink writes the entry of a block into its units (`rav1g-parse/src/sink.rs`); the
  readers of the grid on the CPU, the block lists (`av1_mi_push_block`: the block and its neighbours)
  and the CDEF lists, get the record from the entry and the pool. The clears of the grid, where a
  frame still has them, are half as long.
- *No branch on the size.* The first form of the change (`6635259`) lost the D3D12 device on WARP in
  two streams of every run of the set (run 43774848753). GPU-based validation of that build (run
  43779035065, a throwaway branch) named it: in the loop filter's dispatches, loads from the grid at
  multiples of 8 up to twice its size. `gen_lf_blocks` had `if (cb_mi_grid_entry == 4) one word else
  two`, FXC flattened the branch, and the load of an address ran on the grid of four byte entries
  too. The shaders now have one way for both sizes: the low word at `unit * entry`, the high word at
  `entry - 4` behind it, which with four bytes is the same word. WARP does not bound such loads and
  faults when nothing is mapped behind the buffer, so locally (lavapipe) every frame was right.
- *No load outside the grid.* The unit of a load is held to the last entry of the grid
  (`cb_mi_grid_last`, which both hosts say): the loads of threads without work, which the compiler
  makes unconditional, stay in the buffer, where they read behind it before (they are the overruns of
  root parameter 1 that validation always showed; with a grid of half the size there is less behind
  it that is mapped).
- *A unit without a block.* `gen_pred_blocks` loads the record of a neighbour beside or below the
  frame, whose entry is 0, and makes nothing of it. With an address in the grid that load is where the
  32 bits wrap, outside the pool; with entries of four bytes it would be 4 GiB behind the pool. The
  shader reads the first record of the pool for such a unit when the entries have four bytes;
  `gen_lf_blocks` has always skipped such a unit.
- *The reference.* Branch `cpp` has the same shaders (`736393d`, `c55087e` and `19ad47b`, the pin of
  `rust-equal` since this change): its host writes libaom's grid of addresses and says 8. `stage_dump.py` reads either grid
  by the `entry=` of the dump and compares the records the units name, as before.
- *Checked locally* (wine on lavapipe): the 244 vectors of AOM with the shaders FXC compiled and with
  the shaders DXC 1.9 compiled, 244 of 244 each; the `d2` cut 30 of 30; `rav1g-parse-check` with four
  threads and two frames in parse, 244 of 244 equal; the listings scan and the thread group check of
  the changed shaders.
- **Console, both reductions** (2026-11-15, package 0.2.184.0 against 0.2.175.0, the same task lines,
  slow state, Bale stopped, three rounds of each 4K clip, means of six timed runs): md5 15 of 15.

  | | 0.2.175.0 | 0.2.184.0 | |
  | --- | ---: | ---: | ---: |
  | Device buffers at 3840x2176 10-bit, 3 slots | 646.5 MiB | 593.4 MiB | -53.1 MiB |
  | Application usage, `j4k40`, 30 frames out | 866.3 MiB | 801.9 to 835.0 MiB | about -45 MiB |
  | Decode call, `j4k40` / `d2_300` | 21.79 / 20.01 ms | 21.08 / 19.38 ms | -3.2 % |
  | GPU stage sum, `j4k40` / `d2_300` | 18.64 / 9.48 ms | 18.53 / 9.40 ms | -0.6 / -0.8 % |
  | Frames a second, `j4k40` / `d2_300` | 45.9 / 49.8 | 47.4 / 51.2 | +3.2 / +2.9 % |

  The CPU gains from writing half the grid; the GPU is the same.

### The wait of the second frame in parse in a pool at half (candidate 2)

2026-11-15. **The mode info pool at half is the default of a decoder above 1920x1088** since the rule
below was changed and the console showed no cost (the last item). The decoder for 3840x2160 10-bit (two
frames in parse, three slots) takes **537.6 MiB** of device buffers where it took 593.4.

- *The rule* (the owner's): a configuration that asks for the default (`low_memory` 0 of the C
  configuration, `LowMemory::DEFAULT`, `RAV1G_LOW_MEMORY_DEFAULT`, which `rav1g_settings_default` sets;
  a task of the player or of `rav1g-dec` without `-lowmemparts`) gets the shared back buffer, and the
  mode info pool at half when the decoder is larger than the one frames up to 1920x1088 have (1920x1152,
  540 cells; `default_low_memory` of `create.rs`). Memory is short only at 4K, where half the pool is
  55.8 MiB and the frames are film, which uses a fifth of the pool at most; up to 1080p the whole pool is
  29.5 MiB or less, and the streams that come near the worst case, lossless and low quantizers (58 % in
  the conformance set), are mostly of such sizes: with the pool halved at every size the lossless
  640x360 10-bit vector was a decode error, and with this rule every vector decodes. Nothing is added
  to the path of a frame: the rule is a number at the creation.
- *Parts that are named win*, in both directions and at every size: `-lowmemparts 1`
  (`RAV1G_LOW_MEMORY_SHARED_BACK_BUFFER` alone) is the pool of the worst case at 4K, `-lowmemparts 3`
  the pool at half at 640x360. The `MEMORY ... options` line of a creation says which pool it is and
  why: `mode info pool 1020/2040 of the worst case (the default: half above 1920x1088)`, `540/540 ...
  (the default: whole up to 1920x1088)`, `2040/2040 ... (as asked)`.
- *What the default gives up above 1920x1088.* A frame that needs more than half the worst case of
  mode info (an average block below 8x4 over the whole frame) is a decode error (`MEMORY FAILED frame
  n: its blocks do not fit the mode info pool`), and with two frames in parse a frame that would fit
  alone can fail when its end is much denser than its start (the item on what the rule of the pools
  gives up, below). The largest frame measured takes 12.9 % of the worst case, the most at once
  22.3 %. An application that has to take every stream at every size names the parts without
  `RAV1G_LOW_MEMORY_MODE_INFO`.

Before the change `-lowmemparts 3` was off by default because on the console it cost 2.4 % of the frames a second on `j4k40` and `d2_300` with
the GPU's time unchanged: `cpu wait_sec_slot` of `GPUTIME`, the time a tile waits for a piece of a pool,
went from 0.02 ms a frame to 1.4 to 1.8 ms, in a pool that is used to 20 % at most.

- *The cause* was the rule of the pools ([step 5, "The wait rule"](phase2-rav1d.md#step-5-the-pools),
  `Pools::take`), not a shortage. A frame in parse behind another takes chunks only while what the
  frame in front can still need, and what the frames behind that one hold, fit the pool; "can still
  need" was `held + cells not parsed`, a chunk a cell, which is the worst case. In a pool of the worst
  case that bound is the whole pool when the frame begins and falls with the first cells it parses.
  In a pool at half it stays above the pool until the frame in front has parsed half its cells (1020
  of 2040), so the second frame in parse got its first chunk when the first one was half way through:
  for that long one frame was in parse, not two.
- *The rule now*: in a pool that is smaller than the cells of the frame in front, that frame is left,
  for its cells that are not parsed, their share of the pool (`cells not parsed * pool / cells`,
  rounded up; `Frame::can_need` of `pool.rs`). That is the whole pool when the frame begins and falls
  as it is parsed, as the bound of a pool of the worst case does. A frame whose cells are no more than
  the pool is held against its cells as before, so nothing changes for the default pools, and nothing
  for a frame of 1080p in a decoder for 4K with the pool at half.
- *What it gives up.* A pool below the worst case never held every frame: a frame that needs more than
  the pool is a decode error, as before (the lossless 640x360 vector with the pool at half, before and
  after). New is that a frame which fits the pool alone can fail when it has more than the pool's share
  in its last part, behind a part with less, and the frame behind it has taken the room meanwhile: with
  the pool at half, a last part with more than half a chunk a cell, while the frame behind holds what
  the first part left. The largest frame measured takes 12.9 % of the worst case as a whole. It fails
  as a frame that does not fit does, and its line says what was not its own: `MEMORY FAILED frame n:
  its blocks do not fit the mode info pool (1020 chunks of 256 items, of which the frames in parse
  behind it hold 300)`. A pool at half that never fails a frame which fits alone has to make the frame
  behind wait as it did; there is no rule between the two, because a frame in parse cannot give a
  chunk back before the frame in front of it is submitted.
- *Measured locally* (wine on lavapipe, `-lowmemparts 3`, two frames in parse, `-gputime`; the software
  GPU keeps frames from overlapping in parse most of the time, so the console will show more on both
  sides):

  | Stream | Waits for a frame in front, before / after | `cpu wait_sec_slot` mean, ms | max, ms |
  | --- | --- | --- | --- |
  | `d2` cut, 30 frames | 8 / 0 | 7.035 / 0.008 | 107.0 / 0.04 |
  | `j4k40`, 60 frames | 1 / 0 | 0.007 / 0.007 | 0.010 / 0.010 |
  | the 244 vectors of AOM | 235 / 61 | | |

  With the default pools the `d2` cut has 0.018 ms. The 244 vectors with `-lowmemparts 3 -parseframes 2
  -t 2`: 243 right and the lossless 640x360 10-bit vector a decode error (it does not fit half a pool,
  nothing behind it), the same before and after; with the default pools and two frames in parse 244 of
  244; the `d2` cut with the pool at half 30 of 30.
- *The coefficient pool is only written by the CPU* (it is write-combined memory, `UploadUAV`):
  `decode_coefs` of the parser stores the values that are not zero, and the host clears (the creation,
  a frame that failed). Nothing reads it back; the transform block records, the constants and the
  lists in upload heaps are stored the same way.
- **Console** (package 0.2.185, the owner's app stopped, slow state, three interleaved rounds, `j4k40`
  and `d2_300`, 2 threads, 2 frames in parse, 3 slots; `-lowmemparts 3` against the default of that
  package, 1): md5 15 of 15 and no `MEMORY FAILED`; device buffers 593.4 to 537.6 MiB, the app's usage
  with 30 frames out about 817 to 746..780 MiB; the GPU's time the same; the decode call 21.00 against
  21.02 ms on `j4k40` and 19.31 against 19.27 on `d2`; 47.6 against 47.6 and 51.5 against 51.6 frames
  a second; `cpu wait_sec_slot` 0.017 ms for both. The wait is gone, and with it the cost of the pool
  at half.


- **Console, the default package** (2026-11-15, 0.2.186.0, no memory flag): md5 15 of 15, no `MEMORY FAILED`; the five clips above 1920x1088 got `1020/2040 ... (the default: half above 1920x1088)` and 537.6 MiB of device buffers at 3840x2176 10-bit, the others the whole pool (`240/240`, `540/540`, `4/4`).

### The frame buffer bound at the recording (candidate 5)

2026-11-16. **A part of the low memory parts, in the default above 1920x1088** since the console compared it
(package 0.2.191, [below](#the-frame-buffer-bound-at-the-recording-on-the-console)):
`AV1_LOW_MEMORY_LATE_FRAME_BUFFER`, `RAV1G_LOW_MEMORY_LATE_FRAME_BUFFER`, `LowMemory::LATE_FRAME_BUFFER`,
32, which `-lowmemparts` takes with the others (`-lowmemparts 35` is the default of a 4K decoder with
it; `-lowmem` stays the parts 1, 2 and 4). It has an effect only together with the shared back buffer,
part 1. With it the decoder for 3840x2160 10-bit (two frames in parse,
three slots) takes **513.5 MiB** of device buffers where it takes 537.6: 24.2 MiB less, one frame buffer
(12.1 MiB at 8 bits).

- *What.* A frame of a decoder with more than one frame in parse took its frame buffer from the pool
  when its parse began (the thread of the decode call, `av1_reallocate_frame_buffer`) and counted its
  references on the buffers of the frames it is predicted from at its setup. With the part it gets its
  description only (`describe_frame_without_buffer`: sizes, strides, the same refusal of a frame the
  decoder was not created for), and the thread that records the frames takes the buffer, lays it out
  and counts the references right before the frame is recorded (`take_frame_buffers` of `frame.rs`,
  from `bind_frame_for_record`). So a frame in parse holds no frame buffer, and the pool has 10 where
  it had one more for each slot above two (11 with three slots, 12 with four). With one frame in parse
  nothing changes but the count.
- *Only with the shared back buffer.* A decoder whose frames take their back buffer from the pool
  (`-lowmemparts 0`, or 32 without 1) is left as it is, with its 12 buffers and one for each slot above
  two, and says so (`frame buffers not bound at the recording: that needs the shared back buffer`). The
  first form of the part took that back buffer at the recording as well, and CI showed wrong frames
  with it on WARP (run 43859355280: the set with `-parseframes 3 -t 4 -lowmemparts 32`, and the clean
  stream of the decode error check, differ from their second frame on, while the same runs with 33 are
  right). The local decoder gives such frames without the shared back buffer whatever else is set, with
  one frame in parse and before this change too (147 of the 244 vectors, the same with `-lowmemparts 0`
  and with 32; [rust-port.md](rust-port.md); in the stage dumps of a 16x64 vector the inter
  prediction of the second frame differs at its left and right edge). Whether WARP gives them without
  the part when more than one frame is in parse was not looked at: no job runs that combination. Locally
  it is a matter of the decoder without the shared back buffer, which no default has, and not of this
  part; for WARP that is open.
- *The references.* The entry of a frame in parse already holds a reference on the description of its
  own frame and of the seven it is predicted from (`rav1d_frames.rs`), so every such frame is alive
  until the entry is through; what moved is when the frame buffers behind the descriptions are named.
  The frames are recorded in decode order, so when a frame's turn comes every frame it refers to has
  been recorded, and has its buffer, or has failed, and has the mark, in which case the frame is not
  recorded. The thread of the decode calls no longer asks whether the frame of a reference slot has a
  frame buffer (it may still be in parse), only whether it has the mark (`slot_is_whole`).
- *The paths.* A frame whose parse fails, whose reference failed, or that needs a stage buffer the
  guard refuses never takes a buffer: its slot goes back with none (`release_frame`), and its
  description, which the reference slots and later frames may hold, has the mark and no buffer. A
  setup that fails gives the description back the same way. A `show_existing_frame` is an entry
  behind the frame it shows, so that frame has its buffer when the copy is recorded, or the mark
  ("the frame a show_existing_frame names failed"). A hidden frame is a frame like the others. A drain
  waits for the queue, so every frame that began takes its buffer, is submitted and completes; the
  release of a decoder drains first. Nothing of this is new code but the take itself: the mark and
  the order of the queue were there for step 4.
- *The palette blocks* are written by the tiles in the layout of the frame buffer, which they read
  from the buffer the frame is reconstructed into. That is not the frame's while it is parsed (no
  buffer yet; and with the shared back buffer the description was that of the frame being recorded,
  which only differs for a frame of another size). The slot now keeps the layout of its frame from the
  setup on (`av1_frame_thread_data::layout`), with and without the part.
- *No wait that was not there, and none for a frame behind.* The take waits while the pool is empty,
  as the take at the begin of the parse did. What is out of the pool when the frame at the head of the
  queue asks: buffers the reference slots or the frames of the queue refer to, and buffers only frames
  on the GPU hold. The first are at most 8: each was in a reference slot when the frame at the head
  began, and no frame that began since has a buffer. With the frame's own that is 9 of 10, so a buffer
  the take waits for is always one a frame on the GPU returns, and the completion thread returns it
  without the thread that waits. The floor of 10 is kept: 8 references, the next
  frame, and one that a decoder with one frame in parse needs for a failed frame.
- *The lines.* `MEMORY decoder created options: ... low memory 35 (shared back buffer yes, ..., frame
  buffers bound at the recording)`; `MEMORY decoder created: ... slots 3, frame buffers 10 + 2 back
  buffers`; at the release `MEMORY frame buffers: no buffer for the slots above two; a frame waited n
  times for a frame on the GPU to return one`. `-memstats`: `frame buffers: 10 in the pool ...; out of
  the pool when a frame takes its buffer at its recording, that frame's included: peak ..., typical
  ..., mean ...; a frame waited for a buffer n times` (the waits are counted without the part too).
  The time of the wait is `cpu wait_fb` of `GPUTIME`, on the thread that records.
- *Measured locally* (wine on lavapipe, the defaults against `-lowmemparts 35`, two frames in parse on
  three slots, 60 temporal units of each clip and the 30 of the `d2` cut; the software GPU keeps frames
  in flight longer than the console). The count is taken when the frame takes its buffer, which is
  later with the part, so the two columns are not the same moment.

  | Stream | Default, 11 buffers: out peak, typical, mean | waits | Part 32, 10 buffers: out peak, typical, mean | waits |
  | --- | --- | ---: | --- | ---: |
  | `j4k40` | 9, 8, 7.7 | 0 | 8, 7, 6.7 | 0 |
  | `a1_8b4k_300` | 11, 11, 9.7 | 34 | 10, 10, 8.9 | 13 |
  | `fg4k_300` | 7, 7, 6.7 | 0 | 7, 6, 5.8 | 0 |
  | `t40x8` | 11, 11, 9.2 | 21 | 10, 10, 8.3 | 0 |
  | `t40x1ld` | 5, 4, 4.4 | 0 | 5, 3, 3.3 | 0 |
  | `t12x1` | 11, 11, 9.2 | 21 | 10, 10, 8.2 | 0 |
  | `d2` cut | 11, 9, 8.4 | 7 | 10, 10, 8.1 | 5 |

  A buffer less is out on average, and the pool that is one smaller is found empty less often (18
  times, and 22 in a second run, where the default is 83 times): the frames in parse held the buffers
  the default waits for.
  The streams that empty the pool still do, with their frames on the GPU. The time is
  the same within the noise of the software GPU: two runs each with `-gputime`, 60 frames of
  `a1_8b4k_300` in 8.2 and 8.6 s by default and 8.0 and 7.9 s with the part, `t40x8` in 22.2 and 22.3
  against 22.0 and 21.7 s, the `d2` cut in 6.05 and 6.05 against 6.12 and 6.05 s. `cpu wait_fb` with the
  part is 12 ms a frame on `a1_8b4k_300`, 32 ms on the `d2` cut and 0 on `t40x8`: what the thread that
  records waits for the GPU, which is behind there anyway. The wait of the default, at the begin of a
  parse inside the decode call, has no timer of its own, so the two are compared by the decode call
  and the frames a second.
- *Checked locally:* the frames of the seven streams are the same with and without the part, md5 by
  md5 (and those of the console's references where there is one); the 244 vectors of AOM with
  `-parseframes 2 -t 2 -lowmemparts 33` and with `-parseframes 3 -t 4 -lowmemparts 33`, 244 of 244
  each, and without the part with one and with two frames in parse, 244 of 244; the damaged streams
  of the local set with two and with three frames in parse end as they do without the part.
- *CI* (job `rust-frames`, on WARP): the whole set with `-parseframes 2 -lowmemparts 33` and with
  `-parseframes 3 -t 4 -lowmemparts 33`, and the decode error in the middle of a stream with two and
  three frames in parse and through `rav1g.h`.
- *For the console:* `-lowmemparts 35` against the default on the 4K clips: md5, device buffers 513.5
  against 537.6 MiB and the app's usage, the frames a second and the decode call, `cpu wait_fb` and
  the `MEMORY frame buffers` line (waits), `frame buffers` of `-memstats`. Four of the six clips had
  all 11 out at once there; if the waits do not grow and the time is the same, 32 can join the default
  above 1920x1088 (`default_low_memory` of `create.rs`, `AV1_LOW_MEMORY_DEFAULT`).

#### The frame buffer bound at the recording on the console

2026-11-16, package 0.2.190.0 (883ecee), 2 threads, 2 frames in parse, 3 slots, Bale stopped.

- *md5:* the set of 15 with the part, `-lowmemparts 35` on the clips above 1920x1088 and `-lowmemparts
  33` below: 15 of 15, no `MEMORY FAILED`.
- *Memory:* 513.5 MiB of device buffers at 3840x2176 10-bit where the default takes 537.6 (frame
  buffers 10 + 2 back buffers); 368.4 against 380.5 MiB at 8 bits.
- *Waits for a frame buffer* (`-memstats`, 300 frames): `a1_8b4k_300` 188 by default, 27 with the part;
  `d2_300` 138 and 10; `j4k40` 0 and 0. The default's frames in parse hold their buffers and wait at
  the begin of the parse; with the part the take at the recording waits for the GPU only (`cpu
  wait_fb` 0.002 ms a frame on average, 11.4 ms at most once). Below 1920x1088 the part waits more
  (`b8_720_300` 101, `b8_1080_300` 76 with `-lowmemparts 33`), where it is not in the default.
- *Time* (interleaved, 3 rounds, each the default output and `-output p010`, 6 tasks a side):

  | Clip | fps, default (3) | fps, part (35) | Decode call ms | GPU busy ms |
  | --- | ---: | ---: | --- | --- |
  | `j4k40` | 47.48 | 47.58 | 21.06 / 21.03 | 18.53 / 18.48 |
  | `d2_300` | 51.22 | 51.42 | 19.41 / 19.36 | 9.39 / 9.36 |

  No cost: every difference is within the spread of the rounds (47.06 to 47.98 fps on `j4k40`).
- So 32 is in the default above 1920x1088 (`AV1_LOW_MEMORY_DEFAULT` = 35): the default 4K 10-bit decoder
  takes **513.5 MiB** of device buffers.

### The coefficient pool at four fifths (candidate 6)

2026-11-16. **In the default above 1920x1088 since package 0.2.193**, after the console compared `-lowmemparts 39`
with the default (35), [below](#the-coefficient-pool-at-four-fifths-on-the-console).
The part exists since the pools: `AV1_LOW_MEMORY_COEFS`, `RAV1G_LOW_MEMORY_COEFS`, `LowMemory::COEFS`, 4,
one of the three of `-lowmem`. With it the decoder for 3840x2160 10-bit (two frames in parse, three
slots) takes **501.5 MiB** of device buffers where the default takes 513.5: 12.0 MiB less (356.5
against 368.4 MiB at 8 bits). Nothing of the decoder changed for it but what `-memstats` says.

- *What.* The coefficient pool of the frames in parse is sized for 24 values a 4x4 unit where a chunk
  is sized for 30: 1632 of the 2040 chunks of a 3840x2176 frame, and the 63 for the ends of pieces as
  before, 1695 chunks of 30,720 bytes, 49.7 MiB where the whole pool is 2103 chunks and 61.6 MiB
  (`pool_chunks` of `create.rs`). 24 values a unit is every coefficient of a 4:2:0 frame, 16 of luma
  and 8 of chroma; the other 6 are the padding of the counts (a transform block takes its coefficients
  up to the last one, rounded up to the next multiple of four above it: 20 for a full 4x4 block).
- *Four fifths, not three quarters.* Three quarters would be a new part (1530 + 63 chunks, 46.7 MiB,
  498.5 MiB of device buffers): 3.0 MiB more, for a value in both headers, the C interface, the two
  test programs and their checks, where four fifths is a part that `-lowmem`, `memory_lines.py` and the
  player's smoke test have run since the pools. Measured locally with a build that had three quarters
  under the same part: no stream waited at either size (the table below), so the numbers do not
  choose; the margin does. The most the coefficient pool held at once is 1312 chunks locally and 1211
  on the console (`d2`): 77 and 71 % of the pool at four fifths, 82 and 76 % at three quarters. And
  four fifths has a meaning a stream can be held against, a frame's coefficients without padding;
  three quarters has none.
- *The rule of the pools* is the one of the mode info pool at half
  ([above](#the-wait-of-the-second-frame-in-parse-in-a-pool-at-half-candidate-2)), which is by pool: a
  frame in front whose cells are more than the pool's chunks is left, for its cells that are not
  parsed, their share of the pool (`Frame::can_need`: four fifths of a chunk a cell, rounded up; the
  1632 when it begins). Held against a chunk a cell, the frame behind a 4K frame would get its first
  chunk when a fifth of the frame in front is parsed (remaining cells below 1632); with the share it
  gets it when about 20 cells are parsed, as in the whole pool. A frame of 1080p in the decoder for 4K
  (510 cells) is held against its cells as before. `pool.rs` has a test of the pool of a 4K decoder
  (`the_coefficient_pool_at_four_fifths_leaves_the_frame_in_front_its_share`).
- *A frame that needs more than the pool is a decode error*, as in the mode info pool at half:
  `MEMORY FAILED frame n: its coefficients do not fit the coefficient pool (1695 chunks of 7680 items,
  of which the frames in parse behind it hold m)`, `AOM_CODEC_MEM_ERROR`, and the frames that depend on it fail until
  the next key frame. That is a frame with more than 24 values a 4x4 unit on average over the whole frame
  (the oldest frame in parse has the 63 chunks above the rule too: 83 % of the worst case): nearly every
  coefficient of nearly every block coded, which is lossless or a quantizer near it on noise. The
  densest frames measured: the lossless 640x360 10-bit vector, 74.8 % of the worst case (344,860
  values in 60 cells), which fits; `d2` 33.3 %. And, as in the mode info pool, with two frames in parse
  a frame that fits alone can fail when its last part has more than four fifths of a chunk a cell
  behind a part with less, and the frame behind has taken the room.
- *Why the frame does not wait, and why the pool does not grow.* Both were looked at. A wait gives
  nothing: such a frame is the oldest one when it fails, nothing in front of it holds a chunk, and the
  frames in parse behind it cannot give theirs back before it is submitted (`Take::Full` is that
  state; the take waits in every other). The stage buffers
  [grow](#stage-buffers-sized-for-the-frames-candidate-3) because their counts are known before the
  frame is recorded and one command list uses them at a time. The coefficient pool is written by the
  tiles of every frame in parse through a pointer each tile keeps (`dq_buffer_base`), in the middle of
  a frame whose need is known only when it is parsed, and it is one buffer that the transform stage
  binds and the records address by offset. A larger one would have to be created beside it (49.7 and
  61.6 MiB together, under the budget guard, which may refuse), filled with what the frames in parse
  have written (a read of write-combined memory), and every tile on every parse thread moved to it at
  once: a stop of all parse threads that the parse has no place for. A second buffer for the chunks
  above the first would change the shader's binding. So it stays a decode error. An application that
  has to take such streams at 4K names the parts without `RAV1G_LOW_MEMORY_COEFS`, as for the mode
  info pool.
- *The lines.* `MEMORY decoder created options: ... low memory 39 (..., coefficient pool 1632/2040 and
  63 chunks for the ends of pieces, ...)`. `-memstats` (new with this): `pool idct_coefs: 1695 chunks
  of 7680 coefficients (1632/2040 of the worst case of a frame), 49.7 MiB; chunks in use at once peak
  1312 (77.4 %, 38.4 MiB); ...; a tile waited for a chunk of it 0 times for the GPU to finish an older
  frame and 0 times for a frame in parse in front of its own`. The waits are by pool, which the `MEMORY
  pools` line of a release only has as a sum; the time of all of them is `cpu wait_sec_slot` of
  `GPUTIME`.
- *Measured locally* (wine on lavapipe, the default against `-lowmemparts 39`, and three quarters from
  a build made for the comparison; two frames in parse on three slots, 60 temporal units of each clip
  and the 30 of the `d2` cut). Chunks of the coefficient pool in use at once, peak, as a share of the
  pool; no tile waited for a chunk of the coefficient pool in any of the 21 runs, for the GPU or for a
  frame in front.

  | Stream | At once, peak, chunks | A frame, peak, chunks | Whole, 2103 | Four fifths, 1695 | Three quarters, 1593 |
  | --- | ---: | ---: | ---: | ---: | ---: |
  | `j4k40` | 209 | 70 | 9.9 % | 12.3 % | 13.1 % |
  | `a1_8b4k_300` | 469 | 293 | 22.3 % | 27.7 % | 29.4 % |
  | `fg4k_300` | 46 | 17 | 2.2 % | 2.6 % | 2.9 % |
  | `t40x8` | 554 | 203 | 26.3 % | 32.0 % | 34.8 % |
  | `t40x1ld` | 363 | 167 | 17.3 % | 21.4 % | 22.8 % |
  | `t12x1` | 315 | 126 | 15.0 % | 18.6 % | 19.8 % |
  | `d2` cut | 1312 | 672 | 62.4 % | 77.4 % | 82.4 % |

  The time is the same (two runs each, the first run of a container left out): `j4k40`, `a1_8b4k_300` and the `d2` cut in 23.1 to 23.2, 8.4 to 8.5 and 6.4 s
  by default and 23.3 to 23.4, 8.5 and 6.4 to 6.5 s with the part; `cpu wait_sec_slot` 0.003 to 0.035
  ms a frame on average on both sides, 0.22 ms at most once (the default). The one wait for a frame in
  front in these runs (`d2`, with the part) was for a chunk of transform block records, whose pool is
  whole.
- *Checked locally:* the frames of the seven streams are the same with the default, with the part and
  with three quarters, md5 by md5. The 244 vectors of AOM with `-lowmemparts 37` (the part with the
  whole mode info pool), `-parseframes 2 -t 2` and `-parseframes 3 -t 4`: 244 of 244 each, no `MEMORY
  FAILED`; tiles waited for a chunk of the coefficient pool 59 times with two frames in parse (24 for
  the GPU, 35 for a frame in front; 9 with the whole pool) and 124 times with three, in 9 of the
  vectors, the lossless and low quantizer ones, whose frames take up to 91.6 % of the smaller pool.
  With `-lowmemparts 39` 243 of 244: the lossless 640x360 10-bit vector does not fit half the mode info
  pool, as before, which a decoder of that size does not have by default.
- *For the console:* `-lowmemparts 39` against the default on the 4K clips, `d2_300` first (57.6 % of
  the whole pool at once there, 33.3 % a frame): md5 and no `MEMORY FAILED`; device buffers 501.5
  against 513.5 MiB and the app's usage; the frames a second and the decode call; `cpu wait_sec_slot`;
  and the `pool idct_coefs` line of `-memstats`: chunks in use at once (about 71 % expected on `d2`)
  and the two waits. If the waits stay near 0 and the time is the same, 4 can join the default above
  1920x1088 (`AV1_LOW_MEMORY_DEFAULT` 39, which is then `AV1_LOW_MEMORY_PARTS`), and the default 4K
  10-bit decoder takes 501.5 MiB. If `d2` waits for the GPU there, the pool is the price of those waits
  and stays whole.

#### The coefficient pool at four fifths on the console

2026-11-16, package 0.2.192.0 (cfa49ab), 2 threads, 2 frames in parse, 3 slots, Bale stopped.

- *md5:* the five clips above 1920x1088 of the set and `d2_300` with `-lowmemparts 39 -memstats`: 6 of
  6, no `MEMORY FAILED`. Device buffers 501.5 MiB at 3840x2176 10-bit, 356.5 at 8 bits.
- *The pool* (`pool idct_coefs`, 1695 chunks): in use at once, peak, `d2_300` 1312 (77.4 %; the same as
  locally, not the 71 % the earlier console numbers suggested), `fg4k_300` 621 (36.6 %), `a1_8b4k_300`
  469 (27.7 %), `t40x1ld` 363, `t12x1` 315, `j4k40` 239; a frame at most 700 chunks (`d2`, 41.3 % of the
  pool). No tile waited for a chunk of it, for the GPU or for a frame in front, on any clip.
- *Time* (interleaved, 3 rounds, each the default output and `-output p010`, 6 tasks a side, without
  `-memstats`):

  | Clip | fps, default (35) | fps, part 4 (39) | Decode call ms | GPU busy ms |
  | --- | ---: | ---: | --- | --- |
  | `j4k40` | 47.43 | 47.44 | 21.08 / 21.08 | 18.52 / 18.51 |
  | `d2_300` | 51.15 | 51.30 | 19.48 / 19.40 | 9.38 / 9.38 |

  `cpu wait_sec_slot` 0.016 to 0.023 ms a frame on average on `d2` on both sides. No cost.
- So 4 is in the default above 1920x1088: `AV1_LOW_MEMORY_DEFAULT` is 39, every part
  (`AV1_LOW_MEMORY_PARTS`), and the default 4K 10-bit decoder takes **501.5 MiB** of device buffers
  (537.6 before candidate 5, 646.5 before the reductions of 2026-11-15).
