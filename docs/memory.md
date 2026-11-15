# Memory: what the streams use (`-memstats`)

2026-11-15. A measurement, to choose reductions from: the decoder for 3840x2160 10-bit takes 646.5 MiB
of device buffers in an application with 1 GiB ([phase2-rav1d.md, step 5](phase2-rav1d.md#step-5-the-pools)),
and most of them are sized for a worst case. Nothing of the decoder was changed for it.

- [The flag](#the-flag)
- [The lines](#the-lines)
- [What was measured](#what-was-measured)
- [Where the time of the decode call goes](#where-the-time-of-the-decode-call-goes)
- [Candidates, ranked](#candidates-ranked)

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
| `pool mode_info_pool`, `pool idct_coefs`, `pool idct_blocks_unordered` | The three pools: chunks and size; **chunks in use at once, peak** (what a smaller pool has to hold without a wait); chunks one frame holds when it is recorded, peak and mean; the records (coefficients) one frame wrote, peak, as a share of the pool, and mean; the same at once; **fill**: records written over the room of the chunks held (what the chunk granularity costs). Coefficients are counted as the transform blocks take them, with the padding of their counts. |
| `buffer gen_mi_block_indexes` | The block index list of a slot (CPU, at the parse): entries of a frame against its size. |
| `buffer prediction_blocks`, `buffer prediction_blocks_warp` | The records `gen_pred_blocks` writes on the GPU for a frame against the buffer. The counts are known on the CPU before the frame is recorded. |
| `buffer idct_blocks` | The sorted transform blocks (GPU): as many as the frame's transform block records. |
| `frame buffers` | The buffers of the pool that are out of it (a reference count above 0: named by a reference slot, the frame of a slot in flight, a reference such a frame still reads) at the setup of every frame, that frame's included: peak, the most frequent count, mean, and how many frames saw each count. |
| `references` | How many different frames the 8 reference slots name at the setup of a frame, the same way. |
| `palette` | Palette buffers created; frames that allow screen content tools (each binds a palette buffer of the size of a frame buffer); frames that have palette blocks; the bytes of the palette blocks of a frame (what a palette buffer really holds), peak and mean, and at once. |
| `cpu bytes written` | What the CPU writes for a frame into memory the GPU reads, mean / peak, and by part: mode info records (224 bytes each, cleared and filled), the mode info grid (8 bytes a 4x4 unit), coefficients (an upper bound: 4 bytes for every value up to a block's last one; only those that are not zero are written), transform block records (16 bytes), the block index list, the CDEF lists (record thread), the constants of the stages, palette blocks. Not in it: the counters of the block lists (`gen_block_map`), loop restoration units, film grain offsets, and the clears, which `-gputime` reports. |

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

**Console: pending.** The same task lines with `-memstats` on `j4k40` and `d2` in the slow state, for
the frame buffer counts and the "at once" numbers with a real GPU.

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
| 2 | Mode info pool at half, or a quarter, of the worst case | 55.8 or 83.7 MiB | At once at most 22.3 % (`d2`), 9 to 11 % elsewhere; one frame at most 12.9 %. The wait for the GPU exists and is tested (`poolwait`); a pool at half exists as `-lowmemparts 2`. | Low at half, medium at a quarter. A frame that alone does not fit is a decode error (`MEMORY FAILED`): at half an average block below 8x4 over a whole frame, at a quarter below 8x8; the low quantizer vectors reach 58 % at 352x288. A fallback would be a second buffer created on that frame, under the budget guard. More waits cost speed, not frames. |
| 3 | Stage buffers at a quarter, grown on the frame that needs more: `prediction_blocks`, `prediction_blocks_warp`, `idct_blocks`, and the block index list as a fourth pool | 33.7 MiB (0.75 x 44.9) and 13.5 MiB (0.75 x 18.0) | A frame's peak is 15.8 %, 3.6 %, 26.1 % and 11.5 % (`d2`), a few percent elsewhere. | Medium. The three GPU buffers are used by one command list at a time and their counts are known before the frame is recorded, so a frame that needs more can get a larger buffer first (the guard may refuse: then a decode error). The index list is written at the parse: a pool with a piece for a whole block ([step 5, "Left"](phase2-rav1d.md#step-5-the-pools)). |
| 4 | Smaller mode info record (224 bytes) | up to 55.8 MiB at half the size; less together with 2 | 2.5 to 2.8 MiB of a busy 4K frame's CPU bytes are these records. | Medium. No stream can fail for it, but the layout is shared by the sink, the block lists and the shaders that include `mode_info.h`; what a record can lose is not measured here. |
| 5 | The frame buffer bound at the recording | 24.2 MiB (11 to 10 in the pool) | The reference slots name at most 5 frames in these streams; the pool is emptied by the frames in flight. | Medium: the references of a frame in parse are resolved at the recording, through every error path of step 4. The floor stays 10 (8 references, a failed frame, the next frame): the measured 5 do not allow less, a stream may use 8. |
| 6 | Coefficient pool at three quarters | 15.4 MiB | 62.4 % at once in `d2`, 10 to 26 % elsewhere. | Low, and little: `d2` would be near its waits, and half is not possible for it. |
| 7 | Palette blocks from a pool instead of a buffer of the size of a frame a slot | 0 MiB for film (no clip allows screen content tools); 72.5 MiB less on the first frame of a stream that does | 36 KB a frame at most where a buffer is 3.2 MiB. | Medium: the shaders address the palette buffer like the frame buffer. It removes a growth of 72.5 MiB that the budget guard can only refuse. |

For the CPU (second priority, DRAM bandwidth in the slow state): the mode info grid as 4 bytes a unit
(an index, which is what the shaders use) halves the largest part, 2.0 of 8.6 MiB a frame on `j4k40`, and
takes 2.0 MiB off every slot; the host's readers of the grid (block lists, CDEF lists) then add a base.
The CDEF skip list (0.5 MiB a frame, record thread, read back through the grid) could be a GPU pass.
A smaller mode info record (4) is the other large part.
