# GPU AV1 decoder: history and results

This record started in the Bale repository (<https://github.com/Zorro92/bale-for-silo>), a UWP
media client whose software AV1 path (dav1d) stutters on heavy 4K content on Xbox Series consoles.
It moved here with the decoder. References to `gpu-film-grain.md` and `player-plan.md` are Bale
documents. The test player changes named "patch 0001-0004" below are now the first four commits
after the fork point; see [test-player.md](test-player.md).

## Scoping (2026-11-08)

Goal: make 4K AV1 play smoothly on Xbox Series in app mode. Software dav1d can't: it reaches only
~89% of real time in heavy 4K scenes on ~7 of 8 cores (see gpu-film-grain.md section 8). The console
has no AV1 hardware decoder under D3D11 or D3D12 (player-plan.md), so the only remaining path is
decoding on the GPU with compute shaders. This is a "for science" experiment; production keeps the
1080p AV1 claim until it proves out.

## Reference: av1-xbox-one
Google/AOMedia GPU AV1 decoder for Xbox One (2020–21, last commit 2021-08-25, never upstreamed).
BSD-2-Clause plus the AOMedia Patent License 1.0, so porting is fine if both notices ship with Bale.

- **Split:** CPU does entropy decoding (a modified libaom decoder, ~46.6k lines of C including unused
  libaom recon code). The GPU does everything after it: inverse transforms, intra and inter
  prediction (incl. warp, OBMC, compound, scaled references), loop filter, CDEF, loop restoration,
  super-res upscaling and film grain. 87 HLSL compute shaders, ~15.7k lines; ~5.3k lines of C++ D3D12
  host code drive them.
- **Pipelining:** while the GPU reconstructs frame N, the CPU entropy-decodes frame N+1. The player
  keeps a ~16-frame queue and runs decoder threads above normal priority.
- **No published performance numbers.** It targeted Xbox One in UWP game mode, because D3D12 compute
  wasn't available to apps there. On Series, Bale already creates a hardware D3D12 device in app mode.
- **Shaders use nothing beyond Shader Model 5:** ByteAddressBuffer, groupshared memory, root
  SRV/UAV/constant bindings, no wave intrinsics. So they could run on either D3D11 `cs_5_0` or D3D12.

## Language
The host layer is Rust: a new module or crate next to `bale_gpu`, with a C ABI like the grain and
zero-copy repack code. The heavy work runs in the HLSL compute shaders, reused from the reference. The CPU
entropy decoder starts as the reference's C, built by the Rust crate via `cc`. Replacing it with
Rust (for example rav1d's parser feeding the same GPU buffers) is a later, optional phase, only
worth doing once the GPU side is proven.

## Main questions (answered by phase 0)
1. **D3D11 or D3D12.** D3D11 compute on Bale's shared device writes straight into the MF texture pool,
   with no interop. But the debug GPU repack (since removed) showed that work on the shared immediate context stalls the
   decode thread. A D3D12 compute queue runs independently of the renderer, but its frames must
   reach D3D11 (shared NT-handle textures plus a fence, or D3D11On12). The D3D12 video query also
   removed the device on this console, so D3D12 compute stability needs its own check.
2. **GPU time per 4K frame** in app mode, where the GPU is shared with the shell. Needs a console run
   of the heaviest shaders (inter prediction, inverse transforms, CDEF, loop restoration).
3. **CPU entropy cost at 4K.** libaom's entropy decoder is slower than dav1d's. It must stay well under
   one frame time per frame across the available threads, using tile parallelism. Measure on the
   desktop first, then on the console.
4. **Feature gaps** in a 2021 decoder that was never upstreamed: intra block copy, palette, odd sizes,
   10-bit edge cases. The AOM conformance vectors (8-bit and 10-bit) give md5 checks against dav1d.

## Phases
0. **Spikes.** (a) Build the reference CPU decoder plus shaders in Windows CI and decode AOM
   conformance vectors on WARP, checking md5s. (b) Console timing of representative shaders on D3D11
   and D3D12 with a 4K workload. (c) Desktop measurement of entropy-only decode time on a 4K stream.
   Stop here if the GPU or entropy time can't fit 24 fps with headroom.
1. **Rust host.** Port the D3D12 host code (resources, root bindings, dispatch order, barriers,
   fences) to Rust on whichever API phase 0 picks. md5-exact on the conformance vectors on WARP in CI,
   in the style of the grain and zero-copy WARP tests.
2. **Console decode.** Debug-only path that decodes an AV1 file on the console and reports per-frame
   CPU/GPU time, with no playback yet.
3. **Playback integration.** A wrapper patch that routes AV1 to the GPU decoder instead of dav1d and
   delivers textures into the existing D3D11 sample path. Seeking, flush, resolution changes, HDR10
   metadata and film grain (theirs or Bale's) all need handling. Debug opt-in first, measured with
   the same 120 s Buffering run as the dav1d and (removed) debug repack results.
4. **Optional.** Rust entropy decoder, 10x3 packing for memory, production enablement.

Each phase is gated on the previous one. Phase 0 is the cheap part that decides whether the rest is
worth it.

## Phase 0 results (2026-11-08)
The upstream test player (`uwp_dx12_player`, then built by a workflow in the Bale repository) was
sideloaded and run on the Series console **in app mode**. It decoded 4K 10-bit AV1 unlimited-rate
with `-queue_size 4`. The default 16-frame queue fails to allocate output buffers in app mode.

- **D3D12 compute works in app mode:** the device and all PSOs are created, with shader creation
  taking ~5–6 s. The DEVICE_REMOVED seen with the D3D12 *video* query does not affect compute.
- **Hidden frames hang it:** SVT-AV1 random-access encodes (hidden alt-refs plus
  `show_existing_frame`) stop after the first frame, for both 1 and 8 tiles. Low-delay encodes with
  every frame shown decode fine. Most real AV1 encodes use hidden frames, so this would need fixing
  in any port.

| Stream (3840x2160, 10-bit, 300–600 frames) | GPU decoder fps | Desktop 1-thread dav1d, relative |
|---|---|---|
| Jellyfin 40 Mbps, 1 tile | 10.3 | 1.28 |
| SVT low-delay 40 Mbps, 1 tile | 13.3 | 1.00 |
| SVT low-delay 40 Mbps, 8 tiles | 14.1 | 1.02 |
| SVT low-delay 12.6 Mbps, 1 tile | 15.9 | 0.68 |

For comparison, console dav1d decodes the Jellyfin clip at ~32–35 fps on ~6–7 cores. The GPU
decoder is about 3x slower.

**Reading:**
- Eight tiles at the same bitrate gain only 6%, so tile-parallel entropy decoding is not what limits it.
- Cutting the bitrate 3.2x gains only 19%. Fitting time = fixed + per-bit gives a fixed cost of
  **~57 ms per 4K frame** (≤17.5 fps) even at zero bitrate. 24 fps needs ≤41.7 ms in total.
- The fixed part is likely GPU reconstruction. That is unconfirmed, because the player does not log
  GPU timestamps. It could also be per-frame host overhead such as fence waits or the small queue.

**Gate: not met.** On the reference as built, the per-frame floor alone is ~40% over the 24 fps
budget before any entropy cost, and correctness (spike a) was not checked. Continuing would first
need a GPU timestamp breakdown, which means patching the test player. Only a shader-side fix (a
rewrite for RDNA2 waves, not a port) could close a gap of that size.

**Decision (2026-11-09): stopped after phase 0.** The owner chose not to run the timestamp spike. The
test player was uninstalled from the console. Bale keeps dav1d for software AV1 with the 1080p claim,
and the server transcodes 4K AV1. Since wrapper 0020 that software path is zero-copy direct write only
(player-plan.md).

## Timing spike (2026-11-11)
The owner revived phase 0's GPU-timestamp spike. Test player patch
0002 (`-gputime`, `-nopresent`,
build run 42970282066) was run on the Series console in app mode. Each task decoded 300 frames of 4K
10-bit at unlimited rate with `-t 6 -queue_size 4`. The figures are per frame over 290 measured frames.

| Stream (1 tile) | Present | fps | Wall ms | GPU busy ms (% of wall) | CDEF ms | Intra ms | Inter ms | CPU entropy ms |
|---|---|---|---|---|---|---|---|---|
| SVT low-delay 40 Mbps | no | 13.1 | 76.2 | 66.7 (88%) | 40.6 | 13.4 | 7.1 | 33.7 |
| SVT low-delay 40 Mbps | yes | 12.9 | 77.8 | 67.3 (87%) | 40.8 | 13.6 | 7.3 | 34.5 |
| SVT low-delay 12.6 Mbps | no | 15.3 | 65.2 | 60.5 (93%) | 34.3 | 13.7 | 7.3 | 17.9 |
| Jellyfin 40 Mbps | no | 10.4 | 96.4 | 95.2 (99%) | 50.6 | 21.0 | 18.8 | 44.8 |

CDEF is averaged over all frames. On the frames that run it, it takes 44–54 ms on its own and accounts
for 53–61% of all GPU time. Intra is the wavefront: one dispatch plus a barrier per iteration, 80–190
iterations per frame. Everything else (transforms, loop filter, LR, borders, copies) totals a few ms.

**Reading:**
- **The ~57 ms floor is shader time, not host overhead.** The GPU is busy 87–99% of wall time.
  Command recording takes ~1.6 ms, submission ~0.2 ms and memsets ~4 ms per frame. The idle gaps are
  small.
- **Presentation is not a factor:** 13.1 fps without presenting against 12.9 fps with it.
- **CDEF alone exceeds the 24 fps budget** (41.7 ms) on most frames. Without it the GPU would still need
  ~26 ms for the SVT clips and ~45 ms for the Jellyfin clip, mostly intra and inter.
- **Entropy decoding, in libaom on one core with 1-tile streams, takes 18–45 ms per frame.** It overlaps
  the GPU, but at 40 Mbps it is close to or over the budget unless the stream has several tiles.
- **The hidden-frame hang is output-buffer starvation, as suspected.** On the SVT random-access clip
  the decoder blocks on the first wait with `pool 4, free 0, held by decoder 4`: hidden frames hold
  every output buffer and nothing can free one.
- In app mode the GPU is time-sliced with the system, so these figures include any slice the shell
  takes. A port cannot change that.

**Verdict:** a port as is cannot reach 24 fps at 4K. Getting there would need:
- a new CDEF shader
- a restructured intra wavefront
- a fix for hidden frames: more output buffers, or none rented for hidden frames
- multi-tile entropy decoding, or a faster entropy decoder

That is a decoder project, not a port. Its payoff is playability *and* CPU. dav1d does not hold real
time in heavy 4K scenes either: it stutters on the heaviest parts of a 4K film test, using ~6–7 cores. So a faster GPU
decoder is the only remaining route to smooth heavy 4K AV1, and its entropy side would use about one core.

**Decision (2026-11-11): the owner started a better-decoder project.** The first step is a shader
rewrite inside the existing test player, CDEF first and then the intra wavefront. Each attempt is
measured with `-gputime` and checked bit-exact against the original shaders with the player's
`-save_md5` / `-md5_frame_check`. A Rust port in a new repo comes only after the shaders bring 4K
GPU time well under 41.7 ms per frame.

### 1080p (2026-11-11, same build)
Each task decoded 600 frames of 1080p 10-bit with the same settings.

| Stream | Present | Output fps | Wall ms (p95) | GPU busy ms | CDEF ms | CPU entropy ms |
|---|---|---|---|---|---|---|
| Jellyfin 10 Mbps, no hidden frames | no | 43.6 | 22.9 (36.2) | 21.6 | 12.4 | 12.0 |
| Jellyfin 10 Mbps, no hidden frames | yes | 40.7 | 24.5 (37.5) | 22.3 | 12.6 | 13.4 |
| Netflix Sparks 2.2 Mbps 59.94 fps, 207 of 639 frames hidden | no | 81.6 | 12.3 (58.3) | 11.4 | 3.8 avg | 1.2 |

**1080p24/30 is real time on both clips, with headroom.** On Sparks 1080p60 is real time on
average, but the p95 per output frame is 58 ms and the maximum 235 ms. Playback would need a deep
queue to absorb those spikes, and Jellyfin at 10 Mbps cannot do 60 fps.

**Sparks did not hang** despite its hidden frames. The decoder hit the output-pool limit 23 times
(`held by decoder 4`), but each wait cleared within 25 ms. So the hang depends on the encode's
reference structure; the SVT random-access clip deadlocks, Sparks does not.

**CPU:** entropy decoding costs about 0.3 core at 1080p24 for 10 Mbps and under 0.1 core for Sparks
at 60 fps. dav1d needs under 1 core for 1080p24 and ~3 cores for 1080p60. The GPU is busy for about
half of every 41.7 ms frame at 24 fps.

## Step 1a: CDEF v2 (2026-11-11)
Patch 0003 (Bale build run 42978871382, package 0.1.6.0) adds a rewritten CDEF
shader behind `-cdefv2`:
- One 64x64 group per CDEF block.
- The direction search runs on every thread, unrolled, so it uses no indexable temp; the original
  ran it on 16 of 128 threads through a 120-entry scratch array.
- 16-bit shared memory and a single barrier.
- An early-out for zero-strength blocks.

Same console, same settings as the timing spike, 300 frames of 4K 10-bit:

| Stream (1 tile) | CDEF original ms | CDEF v2 ms | GPU busy ms | Decode fps | Wall ms |
|---|---|---|---|---|---|
| SVT low-delay 40 Mbps | 53.4 | **7.2** | 66.5 → 32.2 | 13.4 → 21.3 | 75.8 → 47.5 |
| Jellyfin 40 Mbps | 51.7 | **5.5** | 95.4 → 50.2 | 10.5 → 16.8 | 96.8 → 59.9 |

**Bit-exact on the console:** `-cdefv2 -md5_frame_check` against `-save_md5` from the original
shader gives `MD5 is OK` on all 300 frames of both clips. A host emulation of both shaders over
1,176 random and edge-case frames also matches (CI job `cdef-check`). The 8-bit path is only covered
by the host check, since there is no 8-bit 4K clip on the console yet.

**Reading:**
- CDEF is solved: 7–9x faster and now 11–17% of GPU time.
- **The GPU is now under budget on the SVT clip.** It is busy 32 ms of a 41.7 ms frame, but the frame
  still takes 47.5 ms, because **the CPU decode thread is now the limit**. It is busy 97% of wall time:
  - entropy decoding: 34 ms (SVT) and 43 ms (Jellyfin), single-threaded on these 1-tile streams
  - setup: 10–12 ms, of which 4 ms is memsets
- **On Jellyfin the GPU is still over budget.** The remaining GPU time is mostly intra (21.2 ms,
  42%) and inter (18.8 ms, 38%).
- **Next GPU target:** the intra wavefront (13.7–21 ms; one dispatch and barrier per step), then
  inter.
- **Next CPU target:** entropy decoding. Options are frame-parallel decoding, multi-tile streams
  only, or a faster entropy decoder (dav1d/rav1d-derived) in the Rust phase. At 12.6 Mbps the SVT
  clip's entropy was 18 ms, so typical film bitrates are less CPU-bound than these 40 Mbps tests.

## Step 1b: intra v2 (2026-11-11, negative)
Patch 0004 (Bale build run 43003822357, package 0.1.9.0) adds `-intrav2`. It is a
new scheduler plus one 384-thread shader that runs several wavefront passes inside each thread
group, so there are fewer dispatches and UAV barriers. The first two CI runs failed: FXC treats
`pass` and `sample` as keywords, and the host check, which compiles the shader as C++, did not
notice.

The figures below are on the same console, both with `-cdefv2`, 300 frames of 4K 10-bit:

| Stream | Intra path | Intra ms (p95) | Barriers per frame | Thread groups | Passes per group | GPU busy ms | Decode fps |
|---|---|---|---|---|---|---|---|
| SVT 40 Mbps | original | 13.1 (22.9) | 83 | 8,096 | 1.0 | 30.7 | 23.1 |
| SVT 40 Mbps | v2 | 31.7 (77.1) | 57 | 1,415 | 3.1 | 50.1 | 16.4 |
| Jellyfin 40 Mbps | original | 21.2 (37.7) | 188 | 7,821 | 1.0 | 50.3 | 16.5 |
| Jellyfin 40 Mbps | v2 | 49.7 (91.5) | 73 | 1,566 | 6.1 | 78.6 | 12.1 |

It is **bit-exact**: `-cdefv2 -intrav2 -md5_frame_check` reports `MD5 is OK` on both clips against
the original shaders. But it is **2.3–2.4x slower.**

**Reading:**
- **Barrier overhead was not the main cost.** The original spends about 0.11–0.16 ms per wavefront
  step, and real content cut steps only 1.5–2.6x. On the host check's random layouts the cut was
  9.5x.
- **Each v2 step is much heavier.** Passes inside a group run in series behind two group-wide
  syncs each.
- **Occupancy is poor.** At 61 temps (up to ~244 VGPRs) only one or two waves fit per SIMD, so
  memory latency is not hidden.
- **The GPU is less full.** There are 5x fewer thread groups.
- **Conclusion:** the intra critical path is latency-bound. Fewer, fatter steps do not help unless
  per-thread cost and register use fall sharply.

**Untried and cheap:** upstream compiles `intra_main.hlsl` and `intra_main_hbd.hlsl` with
optimization **disabled** (`DisableOptimizations` true for Release in `libav1.vcxproj`). They are
the only shaders built that way. With FXC optimization on (an A/B build) the original wavefront
might get much cheaper per step, with the same dispatch structure and the same bit-exact integer
maths.

**The CPU limits the SVT clip anyway.** With `-cdefv2` alone the GPU is busy 30.7 ms per frame
but the wall is 44 ms. The decode thread (entropy 34–38 ms on one tile, plus setup) sets the
pace.

## Step 1c: intra_main with FXC optimisation (2026-11-11, negative)
rav1g commit f96ac20 (CI run 43008653829, package 0.2.3.0) adds `-intraopt`: the same `intra_main`
source compiled with FXC optimisation on, next to upstream's `/Od` build. FXC compiles it without
warnings; the listing is about half the instruction slots (854 against 1616).

Same console, both with `-cdefv2`, 300 frames of 4K 10-bit:

| Stream | Intra `/Od` ms | Intra optimised ms | GPU busy ms | Decode fps | md5 optimised |
|---|---|---|---|---|---|
| SVT 40 Mbps | 13.7 | 14.0 | 32.0 / 32.1 | 21.1 / 21.0 | wrong, all 300 frames |
| Jellyfin 40 Mbps | 21.2 | 21.3 | 50.1 / 50.5 | 16.6 / 16.3 | wrong, all 300 frames |

**Reading:**
- **No gain.** The driver optimises the DXBC itself, so FXC's `/Od` costs nothing. Intra time is the
  latency of its wavefront steps: about 0.11 ms per step on Jellyfin (189 steps per frame) and
  0.16 ms on SVT (83 steps).
- **Wrong and non-deterministic.** Two check runs of the same clip agree on no frame md5, from the
  key frame on. The optimised bytecode exposes a race; the same build without the flag is md5-exact.
  That is very likely why upstream disabled optimisation. Candidates from the listings: a group sync
  FXC removes (`intra_main.hlsl:300`) and the CFL reduction's reliance on lockstep waves.
- **The CPU decode thread is the wall limit on both clips.** Per frame it is busy 45.7 ms (SVT) and
  58.6 ms (Jellyfin) against GPU busy 32 and 50 ms. Entropy is 34 and 44 ms of that; setup and
  command recording are the remaining ~12–15 ms, on the same thread.

## Step 2: pipeline depth and memory (2026-11-11)
rav1g commits 0cd36a3 to cf69c58 (packages 0.2.4.0 to 0.2.7.0) add `-slots N` (frames in flight
between CPU and GPU, default 2), `-asyncclear` (per-frame clears on the GPU completion thread),
`MEMORY` log lines, `-fitsize` (decoder sized to the stream), `-lowmem` (smaller worst-case shares
and a shared back buffer) and a budget guard.

**Memory is the constraint.** A UWP app on the console may use 1024 MiB. The default 2-slot 4K
decoder took the app to 882.7 MiB, ~40 MiB short of the limit with the output pool; a third slot
(+~198 MiB) pushed GPU usage past the budget and removed the D3D12 device for the rest of the
process. After the reductions, app memory after creation is 812.7 MiB (default), 765.5 MiB
(`-fitsize`), 663.4 MiB (`-lowmem -fitsize`) and 773.5 MiB (`-lowmem -fitsize -slots 3`). An embedding
app has the same 1 GiB for everything, so this matters beyond the test player.

Same console, `-cdefv2`, 300 frames of 4K 10-bit; every configuration below is md5-exact:

| Configuration | SVT 40 Mbps fps | Jellyfin 40 Mbps fps | wait for a slot, ms |
|---|---|---|---|
| 2 slots (earlier packages / `-lowmem -fitsize`) | 21.0–22.8 / 21.9 | 16.4–16.7 / 16.4 | 5.6–8.3 |
| 2 slots, `-asyncclear` | 21.0–21.4 | 16.4–16.6 | 7.6–12.0 |
| 3 slots, `-lowmem -fitsize` (2 runs) | 25.6 / 25.3 | 18.7 / 18.6 | 0.3–0.6 |
| 3 slots, `-lowmem -fitsize -asyncclear` (1 run) | 27.3 | 19.2 | 0.4–1.3 |

**Reading:**
- With two slots the CPU and GPU each waited for the other; moving the clears off the decode
  thread only turned them into waiting. A third slot removes the waits: +17% on SVT, +13% on
  Jellyfin. With three slots the moved clears help too (SVT 36.6 ms per frame).
- **SVT now runs above 24 fps.** Its limit is the CPU: entropy decoding on one core, ~34 ms.
- **Jellyfin is GPU-bound** at ~51 ms per frame (intra 21, inter 19); more pipelining cannot help.
- `-lowmem` costs no measurable time. Run-to-run noise is about ±1 fps.
- A destroyed decoder now returns its memory (app usage back to the launch baseline); in md5 runs
  about 75 MiB stays behind, likely the readback buffers, so md5 tests run one configuration per
  launch.

