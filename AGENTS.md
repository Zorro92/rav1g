# Instructions for coding agents

rav1g is a GPU AV1 decoder for a game console (Xbox Series X|S, D3D12 compute, DXC `cs_6_0`) with a
Rust host and rav1d's parse pass. These rules apply to every change in this repository.

## Correctness first

- All code is open to change: shaders, the host, the parse pass, record and buffer layouts, the scheduling
  and the tests. The only rules are strict conformance to the specification and performance within the
  console's limits below. Earlier design choices, the code rav1g was forked from and the layout of
  internal data bind nothing.
- Output must stay bit-exact: every frame's md5 identical to the reference. Any code may change as long
  as that holds.
- CI checks that compare internal state (the stage dumps and CRCs `rust-equal` holds against the C++
  decoder of branch `cpp`) are guard rails from the port, not references. When a change alters an internal
  layout, give the check a documented exception for that stage. Keep the per-frame md5 checks and the
  Argon results as they are: those are the gates. The reference is the AV1 specification v1.0.0 with Errata 1 and the Argon Streams AV1
  conformance set (README, "References"); dav1d is the backup. The code rav1g was forked from
  (branch `original`) is not a reference.
- The public API and ABI (`crates/rav1g/include/rav1g.h`, `tools/rav1g-header/abi.txt`) may change too
  when a change needs it; the project is still in heavy development. Bale builds against them, so such a
  change raises `RAV1G_ABI_VERSION`, records the new fingerprint in `abi.txt`, updates
  `docs/integration.md`, and Bale's thread is told what changed and what it must adjust (a delegated
  agent says so in its report; the parent thread sends the note). Switches for A/B tests are not a reason
  to change the ABI: they go through the internal `av1_*` setters in `libav1/dx/av1_perf.h`.

## Design against the console's resource limits

Check every design decision against the measured limits below and say which limit it is sized for: where
work runs, buffer and pool sizes, threads, CPU against GPU, copies and bit depth. Put this in commit
messages or docs where the decision is made.

Measured on the console for a normal app with the `hevcPlayback` capability (the owner's source of truth
is "Measured limits" in [Bale's docs/gpu-tone-map.md](https://github.com/Zorro92/bale-for-silo/blob/main/docs/gpu-tone-map.md)):

| Resource | Limit |
| --- | --- |
| App memory | 3328 MiB, one pool for CPU and GPU (1024 MiB without `hevcPlayback`) |
| GPU memory | DXGI budget 2857 MiB; D3D12 allocations stop at about 2.8 GB |
| GPU compute | about 1.86 TFLOP/s, a sixth of the console; the queue is not preempted |
| CPU | 8 logical CPUs (4 cores with SMT), shared with the system; no gain beyond 4 threads |
| Idle state | about 10 s after the last input, the state real playback runs in: CPU about 21-25% slower, CPU RAM copies 3.6-4x slower, GPU copies 2.4x slower (97 to 40 GB/s, uploads 68 to 37 GB/s); GPU compute is not slowed |
| GPU features | shader model 6.4 at most, wave64 only, no native 16-bit arithmetic (WARP in CI uses small waves: wave paths are tested only on the console) |
| D3D12 pipeline caches | `ID3D12PipelineLibrary` and `GetCachedBlob` remove the device: don't use them |
| Dependent dispatches | about 30 us of GPU time per step of a chain of dependent compute dispatches |

**rav1g's envelope during 4K AV1 playback in Bale** ("Now: rav1g's envelope" in
[Bale's docs/resource-budget.md](https://github.com/Zorro92/bale-for-silo/blob/main/docs/resource-budget.md)).
It is a ceiling, not a target: the goal is the smoothest playback, and at equal performance less usage.

| Resource | rav1g may use at most |
| --- | --- |
| Memory, all of rav1g (buffers, output images, device) | 1.6 GB soft, 2.0 GB hard; GPU-resident up to about 2.0 GB |
| GPU | 85% of the app's slice: per frame at most 35 ms at 24 fps, 28 ms at 30, 17 ms at 50, 14 ms at 60 |
| CPU | 3 cores, and no gain beyond 4 threads in total |

Measure performance in the idle state. Spend memory only where it buys measured idle-state frame rate or
removes stalls (for example a deeper queue that absorbs heavy hidden frames).

What follows from them:
- Heavy per-frame work belongs in GPU compute. The CPU touches a whole frame at most once.
- Move fewer bytes: copies (CPU and GPU) are what the idle state slows. Prefer writing results in place
  from compute over extra copy passes.
- Use at most 4 CPU threads for a decoder.
- The priority order: (1) correctness, bit-exact to the specification and Argon; (2) performance, smooth
  real-time playback, the heaviest 4K content first, judged in the idle state on the console; (3) stay
  within the envelope above, which is a ceiling, not a goal. Memory and bandwidth matter only where they
  reach that ceiling or cost speed: at equal performance less is better, and memory that buys measured
  frame rate is fine. A 4K 10-bit decoder with 4 slots is 512514816 bytes of device buffers today.
- Keep the source bit depth (8-bit stays 8-bit, more than 8 becomes 10-bit, never 10 to 8).

## Repository rules

- Never commit IP addresses, tokens, local absolute paths, media titles, test clips or Argon data.
- Commit messages cite the specification section a fix follows.
