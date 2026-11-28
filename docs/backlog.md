# Backlog

Work that is known to be worth doing, in order of what it would do for playback. Each entry says what
limits playback today, what is measured, and where to start. Done items leave the list (git keeps them).

## 1. The CPU parse of high-bitrate one-tile 4K (2026-11-24)

**What limits playback.** A one-tile stream's entropy decode is serial within a frame, so on high-bitrate
4K the frame rate is the CPU time of a frame on one thread, not the GPU. The GPU has room: a 4K HDR10
frame at 60 fps keeps it busy about 10-11 ms, display pass included, of the 14 ms of rav1g's envelope
(`docs/integration.md`, "The display pass"; [stage-fusion.md](stage-fusion.md)). The remaining drops
are single heavy frames, which more frames in parse cannot split:

- Key frames and the hidden frames of the frame pairs: on a 46 Mbps 10-bit 4K50 stream 190-324 ms of
  entropy decode a key frame and 115-155 ms for each hidden frame of a pair, against 8-18 ms of GPU
  (console, idle state, 2026-11-21, before the parse changes of `v0.7.0-alpha.1`).
- Since `v0.6.0-alpha.1` (four parse threads and four frames in parse above 2560x1440) that stream
  plays with 4 late frames in 300, all at key frames; a 40 Mbps 10-bit 4K60 stream with none; a
  100 Mbps 4K60 stream decodes at 63.9 frames a second at full speed, little headroom for the idle
  state ([integration.md](integration.md#releases)).
- In Bale (`v0.8.0-alpha.1`, a 4K AV1 HDR10 film at 23.976 fps, 45 s): 0.2-0.9 % late frames and one or
  two short stalls near the start.
- The idle state slows the CPU by 21-25 % and CPU memory copies 3.6-4x, the GPU's compute not at all
  (AGENTS.md): what is close on the CPU now is what breaks first in real playback.

**Where the time goes.** rav1d's parse ran about 1.4 times dav1d's instructions outside the shared msac
assembly on a 40 Mbps one-tile 4K stream (`perf record -e instructions:u`, laptop, 2026-11-23): the
context and record updates of a block (`decode_b`) about twice dav1d's, the loops that call
`decode_coefs`, `decode_sb` and refmvs 1.14, `decode_coefs` itself 1.07; the token loops spill about ten
values across each call of the assembly (six callee-saved registers). The first changes from that
comparison (context updates, coefficient contexts, the motion vector sort: AV1 5.11.39, 7.10.2, 7.12.3,
8.3.2) are on `main` since `v0.7.0-alpha.1` (-2.3 % instructions); the full write-up is on the tag
`archive/exp/parse-speed-opus` (`docs/phase2-rav1d.md`, "rav1d's parse against dav1d's, instruction by
instruction").

**dav1d's newer releases do not close the gap.** Its parse-side changes from 1.5.1 to 1.5.5 and master
were ported or measured on 2026-11-25 (`third_party/rav1d/PATCHES.md`, "From dav1d"): the temporal motion
vector runs gave 0.10-0.16 % fewer instructions on the 4K streams; the msac assembly change, the load_tmvs
assembly and the branchless negations did not lower the instruction count (the msac change ran 0.4-0.6 %
more instructions and 3.3 % more cycles on an Intel laptop; only a console A/B could still show a gain from
its store-forwarding fix); the decode_b changes are in dav1d's reconstruction pass, which rav1g does not
have. The gap is in rav1d's own Rust code.

**Where to start.**

1. Fewer instructions on the parse thread: the block context and record updates of `decode_b` towards
   dav1d's, and the register pressure of the token loops. The gate is the instruction count of
   `rav1g-parse-check -time` (stable from run to run) and the parse digest; the console's idle state
   gives the time.
2. Absorb a single heavy frame instead of making it faster: the depth of the output queue Bale keeps
   (a deeper queue took one stream's drops from 59-78 to 16-18 in 300 on 2026-11-21) against memory,
   1.6 GB soft in rav1g's envelope, and the start-up delay it adds.
3. What a key frame waits for between parse threads (CDF and reference waits) on the console, with the
   per-frame times of `av1_perf_frame` joined to Bale's log by `user_data`.

Measure on the console in the idle state, paced playback first (late frames in 300 or 1200), full speed
second; the heaviest one-tile 4K streams first (the 40 Mbps bar, the 60 Mbps clip as headroom).

## Smaller items

- **The display pass's PQ 2:1 cost** rose 0.10 ms (1.49 to 1.59 ms a 4K frame) with the block paths'
  chroma window, with the same 2:1 instructions in the listing; two pipelines a mode did not help
  ([stage-fusion.md](stage-fusion.md), "Tried: two pipelines a mode"). 0.6 % of a 60 fps
  frame: left unless the GPU comes near its ceiling.
- **The CI colour job's ffmpeg download** hung past its timeout twice (2026-11-23, 2026-11-24); a rerun
  passed both times. A retry or a cached copy would stop the false failures.
- **`intrav2` / `interfast`** (public settings): keep or retire with the next interface change.
- **The pipeline states a stream needs, first.** `rav1g_device_create` compiles all 105 pipeline states
  (about 6.5 s and 77 MiB on the console; a disk cache removes the device there, integration.md). A 10-bit
  stream without film grain, super-resolution, scaled references or the display pass uses about half:
  compiling those first and letting `rav1g_decoder_create` wait only for them would roughly halve the
  first-open wait; leaving the rest uncompiled until a stream needs them would also save about 30-40 MiB,
  at the cost of a compile (0.1-0.5 s) on first use mid-stream. Needs readiness per pipeline at every
  dispatch site and a test mode that catches a missing one. Bale prewarms the device when an AV1 title's
  Detail page opens, which hides the 6.5 s today; retiring `intrav2` / `interfast` drops 9 of them
  (`intra_v2`, `intra_main` and the `inter_fast` family with `gen_pred_blocks_fast`, both bit depths;
  `-intrafilterlegacy` took one with it on 2026-11-27).
