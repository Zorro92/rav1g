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

## Step 3: defaults and hidden frames (2026-11-11)

**Defaults.** What step 1a and step 2 proved is what the test player and the library run without
flags: CDEF v2, a decoder sized for the stream, one back buffer shared by all frames. The 4K
10-bit default decoder is 615.7 MiB of D3D12 buffers (computed), against 681.9 MiB before. Three
slots, the smaller shares of `-lowmem`, `-asyncclear` and both intra experiments stay behind
their flags.

**Hidden frames.** The hang of phase 0 and of the timing spike had one cause: the decoder took an
output buffer for every hidden frame that can be shown later, at the time it decoded the frame,
and kept it until the frame was shown. A random-access stream starts with as many hidden frames
as the test pool had buffers. An output buffer is now taken only for a frame that is output, at
that moment: a shown frame at the end of its decode, a frame shown with `show_existing_frame` in
a small command list of its own that copies it from its frame buffer (with film grain). Details
and the console checks are in [test-player.md](test-player.md#hidden-frames-show_existing_frame).
Written without a console; md5 references for these streams come from dav1d, since the original
path cannot decode them.

## Step 4: global motion (2026-11-11)

**The bug.** A 1080p 10-bit stream (4 tile columns, many hidden frames) was md5-identical to
dav1d except in the frames with global motion and those predicted from them; none of the four 4K
test clips has global motion. First wrong output: frame 525, a rotzoom model on ALTREF, 584 luma
samples off by up to 436 in one 32x16 block near the right edge (x 1856, y 624), about 140 per
chroma plane. Upstream behaviour: the same md5 with `-cdefv1 -nofitsize -lowmemparts 0` and with
two or three slots.

**The cause.** Not the frame edge. The block is `GLOBAL_GLOBALMV` with a wedge: LAST without
global motion, ALTREF warped. The warped half was right. The other half was the top left corner
of LAST, sample for sample: the shader for these blocks (`inter_scale_warp_compound`) sends a
reference that is not warped through `scale_value()`, which computed position times scale in
double precision, and the console returned 0 from it, so the block position was lost and only
the offset inside the block was left. The console accepts shaders with double precision without
an error. All twenty `inter_scale_*` shaders used it; the other nineteen only run for scaled
references.

**The fix.** `scale_value()` in integers: the scale split into its upper bits and its lowest
eight, so both partial products fit 32 bits, rounded like libaom's `scaled_x()`. Same results as
working double precision, so nothing changes on a GPU that had it.

**The check.** `testing/warp_check` runs the warp shaders (one reference with a local or global
warp, `GLOBAL_GLOBALMV`, 8-bit and 10-bit) as C++ against libaom's C code of this tree on
generated frames, whole pool compared, each frame with double precision working and with it
yielding 0. Before the fix: 0 of 381 frames differ with double precision, 247 without, the block
of the console failure among them. After: 0 and 0, and nine mutated shaders are rejected. With
working double precision the upstream warp shaders match libaom in every case tried: edge clamps,
border width, shear, rounding, masks.

Written without a console. The md5 run against dav1d (1000 frames of the 1080p stream, the four
4K clips unchanged) is in [test-player.md](test-player.md#console-run).

## Step 3: correctness against dav1d (2026-11-11)
Per-frame md5s from dav1d (`ffmpeg -c:v libdav1d -f framemd5`, field 6) are byte-identical to the
test player's own, so dav1d is now the reference (`testing/md5ref/make_md5_ref.sh`); the dav1d
reference of a 300-frame 4K clip equals the one saved from the original shaders. That covers streams
the original path cannot decode and replaces "same as upstream" with "same as dav1d".

- **Hidden frames** (rav1g 362713b): upstream rented an output buffer for every hidden showable
  frame, so a random-access stream with four hidden frames per group deadlocked a pool of four.
  Hidden frames now take no output buffer; a show-existing frame gets its copy (and film grain) in
  its own small command list, ~2.3 ms of GPU at 4K. On the console the SVT-AV1 random-access 4K clip
  is md5-identical to dav1d: 27.9 fps (2 slots) and 29.4 fps (3 slots) at 12.6 Mbps, against
  26.0 / 29.8 fps for the low-delay encode of the same source.
- **Global motion** (rav1g 51c7c15): in a 1080p stream every frame with global motion was wrong,
  and frames predicted from them. The pixel diff of the first bad frame against dav1d
  (`-save_wrong_yuv`) was one 32x16 GLOBAL_GLOBALMV block whose non-warped half came out as if at
  position 0,0: `scale_value()` in the scaled/warp inter shaders computed in double precision, and
  the console returned 0. It is integer arithmetic now, bit-identical to libaom
  (`testing/warp_check`). On the console 1200 frames of that stream, and 1080p and 4K film-grain
  streams with global motion and show-existing frames, are md5-identical to dav1d. The 20 scaled
  inter shaders shared the double-precision code, so streams with scaled references were probably
  wrong too; untested.
- **8-bit, open**: an 8-bit 4K stream (no global motion, hidden frames) is wrong from the key frame:
  luma only, ±1–2 on ~0.4% of the pixels across the frame, chroma exact. The wrong md5 is the same
  with the original CDEF, the old buffer sizes and 2 or 3 slots, so it is upstream behaviour. All
  earlier test clips were 10-bit.

## Lockstep reliance (portability) (2026-11-11)

The first conformance run on the WARP software adapter (`testing/warp_decoder`,
[warp-decoder.md](warp-decoder.md)) had 0 of 250 streams right, with another md5 on every
decode. The cause is in upstream's shaders, not in WARP.

**What was found.** The threads of a compute shader thread group hand data to each other through
group memory (`groupshared`). D3D12 promises one thing about them: at
`GroupMemoryBarrierWithGroupSync()` all threads of the group have arrived before any goes on.
`GroupMemoryBarrier()` makes writes visible and makes no thread wait. Upstream's shaders used the
plain barrier, or none, wherever a thread reads what another thread wrote, and so counted on the
64 threads of a group running in lockstep, instruction by instruction, as one wave of 64. That is
how the console's GPU runs them. It is not how WARP runs them (one thread after the other), nor a
GPU with waves of 32 (NVIDIA, Intel, AMD in wave32 mode): there a group of 64 is two or more waves
and the hand-over is a race. Of the 92 shaders, 58 have group memory; 53 of them had such a race
(`testing/sync_check` on the shaders of the commit before the fix reports them all). The other
five use the memory as private scratch of a thread (`inter_warp`, `inter_warp_hbd`), not at all
(`inter_2x2chroma_hbd`, `inter_compound_2x2_hbd`), or already had group syncs
(`film_grain_filter`), as `cdef_filter`, `cdef_filter_v2` and `intra_v2` do.

Lines are those of the upstream sources (rav1g 7ff22d5):

| shaders | where | what relied on lockstep |
| --- | --- | --- |
| the 19 transforms `idct*x*` | `idct_shader_common.h:86, 94, 107` (macro `IDCT_GEN`) | Coefficients are placed by all threads of a block, rows are transformed by one thread each, then columns, then the block is stored: three plain barriers between the four passes. `:55`: a thread behind the last block returned before them. |
| `idct_lossless` | `:50, 74, 92`, return at `:28` | the same, for the 4x4 Walsh-Hadamard transform |
| `inter_scale`, `_obmc_above`, `_obmc_left`, `inter_scale_2x2chroma_hbd`, and their `_hbd` builds | one plain barrier each (`inter_scale.hlsl:92`) | The horizontally filtered lines of a column go from the thread that made them to the threads that filter vertically. |
| `inter_scale_compound`, `_masked`, `_diff_chroma`, `_2x2_hbd`, and `_hbd` builds | three plain barriers each (`inter_scale_compound.hlsl:106, 128, 170`) | the same for two references, which reuse the same memory |
| `inter_scale_2x2chroma`, `inter_scale_compound_2x2` | `:121, 123` and `:201, 203`, inside `if (block.y & DualWriteBlock)` | Two 2x2 blocks that share a word of the frame exchange their halves; the barriers were in a branch only some threads take. |
| `inter_2x2chroma`, `inter_compound_2x2` | `:115`, `:173`, in the same branch | the same pair write, in the shaders for references of the frame's own size |
| `inter_compound_diff_luma`, `_hbd` | `:252`, `:215` | The mask sums of a 4x2 go to the neighbouring thread, which stores both as the chroma mask. |
| `inter_scale_compound_diff_luma`, `_hbd` | `:114, 136, 178, 223, 229` | two references and the mask sums |
| `inter_scale_warp_compound`, `_hbd` (compound blocks with global motion) | `:119, 167, 188, 247, 295`, inside the branches for a warped and a plain reference; no barrier at `:382`–`:392` | two passes per reference; the mask sums of a difference weighted block were written to and read from the group memory with nothing between |
| `inter_warp_compound`, `_hbd` (not used by the decoder) | `:94`–`:134` and three more places, `:345`–`:351` | the same, without any barrier |
| `loopfilter_hor`, `_hbd` | `:154` in `filter_horizontal_edge`, called from a loop over the edges of the thread's own block (`:218`–`:231`); `:211` | The four threads of a block filter their columns, then put the lines together from all four. Every block has its own number of edges, so the loop had a trip count per thread, and an edge reads from the frame what the edge above it stored. |
| `loopfilter_vert`, `_hbd` | `:182`, `:184` | the edge list of a block, loaded by its four threads |
| `loop_restoration` | `:276`, and no barrier at `:297`–`:314` (Wiener rows to columns to stored words), in `box_filter0` and `box_filter1` (`:91`–`:200`: box sums of the self guided filter, read by their neighbours), at `:349`–`:350` | six passes, one plain barrier |
| `film_grain_gen_luma` | `:81`–`:104` | The autoregressive filter of the grain template is a wavefront: the thread of a row starts four columns after the thread of the row above, by skipping loop iterations, with no barrier in the loop. |
| `film_grain_gen_chroma` | `:94, 102`, and the same wavefront at `:118`–`:156` | |
| `intra_main`, `_hbd` | `:424`–`:444` | The sum of the luma of a CFL block was a pairwise reduction with plain barriers in loops with a trip count per thread ("count ... up to 64 == wavefront size, => we can use GroupMemoryBarrier()"). |
| `intra_filter`, `_hbd` | `:95, 111, 132, 156`; `:166`–`:180` | Filter intra is a wavefront over 4x2 units with plain barriers in a loop with a trip count per thread. And the residual was added by all 8 thread rows of the group to the same words: right only if all of them load before any stores. |

The same assumption is behind the earlier `-intraopt` result ([step 1c](#step-1c-intra_main-with-fxc-optimisation-2026-11-11-negative)).

**How it was fixed** (rav1g c35d97f and the commit after it). The arithmetic and its order are
unchanged; only where threads wait for each other changed.

- Every hand-over has a `GroupMemoryBarrierWithGroupSync()`. FXC only accepts a group sync that
  all threads of the group reach (error X3663/X4026 otherwise), which shaped the rest.
- A thread behind the last block of a dispatch used to return at once. It now takes part in the
  group syncs and does nothing else: it loads nothing and stores nothing.
- A group sync cannot be in a branch only some threads take. The pair write, the two branches of
  the warp compound shaders and the film grain branches were split so that the sync is between
  them; where the code before the sync is long (`inter_2x2chroma`, `inter_compound_2x2`,
  `inter_compound_diff_luma`) it became a function, textually unchanged, and the sync follows the
  call.
- A loop with a trip count per thread cannot contain a group sync. The horizontal deblocking
  filter now steps all blocks of a group through their edges together: every block counts its
  edges, the largest count of the group is read behind a group sync, and a block with fewer edges
  idles through the rest. The wavefronts of filter intra and of the film grain templates became
  steps of the whole group in which a thread whose row has not begun or is done does nothing.
- The CFL sum is made by the first thread of the block between two group syncs. The filter intra
  residual is added to every word once.
- Loop restoration takes its group from `SV_GroupID`, so that the filter type, which is the same
  for a group, is not thread dependent for the compiler, and has a group sync between its passes.

**Checked.** `testing/common/group_sched.h` runs the threads of a group on the host as fibers:
between two group syncs every thread runs alone, in an order drawn anew every time, a plain
barrier does nothing, and the group memory records who read and wrote each element since the
last group sync, so that every race is reported with its source line, whatever order was drawn.
`cdef_check`, `intra_check` and `warp_check` run their shaders this way (the warp compound shader
still equals libaom, the fixed intra shaders still equal `intra_v2`), and `testing/sync_check`
runs all 58 shaders with group memory on generated input: no race, every group sync reached by
all threads, no plain barrier, output independent of the thread order. As a proof that the checks
can fail, each of the 113 group syncs is turned into a plain barrier in a copy of its shader: 111
copies are rejected for a race, and 2 pass, which are syncs of upstream's `film_grain_filter`
between passes that touch only a thread's own samples. On WARP the conformance run went from 0 to
245 of 251 streams ([warp-decoder.md](warp-decoder.md#results)).

**Cost.** Static FXC counts of the 59 shaders that changed: 39545 instruction slots before,
40483 after (+2.4%); the survey of every CI run has them per shader. The largest relative changes
are in small shaders: `idct_lossless` +13%, the 20 shaders for scaled references +7 to +11%,
`loopfilter_hor` +8 to +9%, the transforms -0.5 to +8%. What a group sync costs on the console is
not known: a group of 64 threads is one wave there, where a sync should be free, and `intra_main`
(256 threads, four waves), where it is not, has the twelve group syncs it had before.

**Console (package 0.2.19.0, rav1g b9a9584).** Output unchanged: md5-identical to dav1d on the 4K
10-bit clips (`c35d97f`), and on the final build on 1080p and 4K film-grain clips, a 4K low-delay clip
and 600 frames of Sparks. Frame time unchanged within noise, `-gputime`, 300 frames of 4K 10-bit,
same settings as package 0.2.9.0:

| Clip | fps before → after | GPU busy ms | idct | inter | intra | lf_horz |
|---|---|---|---|---|---|---|
| SVT random access 12.6 Mbps | 27.9 → 28.5 | 34.24 → 33.57 | 0.668 → 0.669 | 10.46 → 10.23 | 12.42 → 12.24 | 0.380 → 0.344 |
| SVT low-delay 12.6 Mbps | 26.0 → 25.9 | 33.40 → 33.36 | 0.649 → 0.653 | 7.77 → 7.74 | 14.17 → 14.15 | 0.408 → 0.395 |
| Jellyfin 40 Mbps | 16.4 → 16.5 | 51.61 → 51.46 | 0.818 → 0.817 | 19.47 → 19.60 | 21.65 → 21.67 | – |

**Open questions for the console run.** Two things that were wrong on the console are right on
WARP with the fixed shaders: the 8-bit 4K stream whose luma was off by ±1–2 (20 frames,
md5-identical to dav1d) and the stream with scaled references (64 of 64 frames). Either they
depend on the console itself, or the console does not run every group in lockstep after all and
the fix repairs them there too. **Answer:** they depend on the console. With the fixed shaders the console still gets the 8-bit
4K stream wrong on 300 of 300 frames, two 8-bit Chimera cuts on 298 of 300, and the scaled-reference
stream on 53 of 64, as before. Something in those paths gives a different result on the console's GPU
than on WARP (as the double-precision helper did for global motion).

*Correction, 2026-11-12:* that holds for the 8-bit streams only
([the cause](#a-paired-read-before-an-overlapping-in-place-write-2026-11-12)). The scaled-reference stream was never wrong on the console: its md5 references were
(see [below](#console-only-mismatches-located-by-stage-dumps-2026-11-12)).

## Bit fields of width 0 (2026-11-12, candidate fix, not yet run on the console)

What is wrong on the console and right on WARP: 8-bit streams (luma off by ±1–2 on about 0.4%
of the samples from the key frame on, chroma exact) and frames that predict from references of
another size. The wrong output is the same with every option, with other buffer layouts and
before and after the group syncs, so it is not a race: something computes another value on that
GPU. No shader has floating point: the FXC listings of all shaders have no float or double
instruction at all.

What the listings do have is bit field instructions (`ubfe`, `bfi`) whose width is a run-time
value. FXC makes them out of `x & ((1 << n) - 1)`, alone or multiplied by a power of two, and out
of `(x >> n) << n`, when `n` is not a constant. D3D defines them for a width of 0 (`ubfe` gives
0, `bfi` inserts nothing), and WARP does that. The shaders where such an instruction can meet a
width of 0 and is not the plain `ubfe` at offset 0 are exactly the ones that are wrong on the
console:

| Shader | Source | Instruction | Width 0 when |
|---|---|---|---|
| `intra_filter` (8-bit only; `intra_filter_hbd` masks with `(bw << 1) - 1`) | `4 * (idx & (bw - 1))`, `8 * (idx & (bw - 1))`: the word of a block the residual is added to | `bfi` width `bw_log2`, offsets 2 and 3 | the filter intra block is 4 samples wide. Filter intra is luma only |
| all `inter_scale_*`, 8-bit and 10-bit | `(thread.x >> 2) & ((1 << (w_log + h_log)) - 1)`, `4 * (subblock & ((1 << w_log) - 1))` | `ubfe` width `w_log + h_log` offset 2, `bfi` width `w_log` offset 2 | the block is 4x4, or 4 samples wide |
| `inter_compound_masked(_hbd)`, `inter_warp_compound(_hbd)` | the same expressions | the same | chroma of an 8x16 or 8x32 wedge block (masked); never (warp: 8x8 and larger) |
| `inter_ext_borders(_hbd)` | `(1 << shft) - 1` as the mask of the samples that stay in the last word of a row | `bfi` width `shft`, offset 0 | the plane's width is 4n + 1 (8-bit) or odd (10-bit): a chroma plane of the scaled-reference stream is 591 wide |

The inter shaders of frames whose references have the frame's size use the same idiom at offset
0 (`thread.x & ((1 << (w_log + h_log)) - 1)`, `ubfe` with a run-time width at offset 0), with a
width of 0 for every 4x4 block, and are md5-identical to dav1d on the console: that form is
right there. `intra_main` is built with `/Od` and has no bit field instruction at all; its
optimised test build (`-intraopt`), which has `bfi` and `ubfe` with run-time widths at offsets 2
and 3, was wrong on the console.

So the working assumption is that a `bfi`, or a `ubfe` at a non-zero offset, with a run-time
width of 0 does not give the D3D result on the console's GPU. It fits the observations (8-bit
only and luma only; errors of the size small residuals have at that quantizer; scaled references
only; WARP right; the same output with every option), but it is an inference from the listings,
**not confirmed on the console**.

*Result, 2026-11-12:* it was not the cause. The console's output did not change with the table masks;
the 8-bit mismatch is one instruction of the vertical loop filter ([below](#a-paired-read-before-an-overlapping-in-place-write-2026-11-12)), and the
scaled-reference stream was right all along (wrong md5 references). The table masks stay: the results
are the same and the survey's rule costs nothing.

**Change.** The masks come from a table (`low_bits()` in `inter_common.h`, local tables in
`intra_filter.hlsl` and the two border shaders), which FXC compiles to a table lookup and an
`and`: 23 shaders, same results (the conformance run on WARP and the host checks are unchanged).
The workflow's shader survey now fails on a bit field instruction with a run-time width other
than `ubfe` at offset 0. If the console decodes the 8-bit and the scaled-reference streams right
with this build, the assumption holds; if not, the stage dumps
([test-player.md](test-player.md#stage-dumps--dumpstages)) say which stage differs.

## Layers, new sequences, mixed reference sizes; all conformance streams pass on WARP (2026-11-12)

The last changes to the C++ decoder before the port: it is what the Rust decoder will be
checked against, so the five conformance streams that still failed on WARP had to be right, or
wrong for a known reason.

| Stream | Was | Cause | Now |
|---|---|---|---|
| `av1-1-b8-22-svc-L2T1`, `-L2T1-2`, `-L2T2`, `-L2T2-2` (two spatial layers, 640x360 and 1280x720) | 16 frames for 8 temporal units | every shown frame was output; the output process (7.18.1) of a displaying application, libaom and dav1d output the highest spatial layer of a temporal unit | 8 frames |
| the same | of the 1280x720 frames only the first right | `inter_warp(_hbd).hlsl` clamped a warped block to the size of reference 0 (here the 640x360 frame) instead of its own reference's | md5-identical |
| `av1-1-b8-03-sizeup.mkv` (352x288, then 1280x720) | decode error at temporal unit 10 | the decoder is created for the first sequence and cannot grow | `AOM_CODEC_NEEDS_RECONFIGURE`; the application replaces the decoder; md5-identical |

With these, **251 of 251 streams are md5-identical on WARP** (run 43149171146; before: 246),
with each experiment flag and with the uavsync shaders too, and `expected.txt` is empty. The
details are in [test-player.md](test-player.md#layers-and-new-sequences) (the output rule, the
controls, the reconfiguration and why the application does it) and under
[the warp check](test-player.md#host-side-check-of-the-warp-shaders) (the clamp, the rule
against warping a scaled reference, and the hint about 4-wide difference weighted blocks that
was not a defect).

Also in this round:

- **Global motion and scaled references.** A `GLOBALMV` block was warped whenever its reference
  had a model; libaom and the specification do not warp a scaled reference. Fixed in the host
  (`td->gm_warp`). No stream of the conformance run has both, so no output changed.
- **Warnings.** C5208 (the unnamed `compute_shader_lib` struct) and the four X3556 of
  `loop_restoration.hlsl` (signed division and remainder of values that are never negative):
  505 → 490 instruction slots, same output. The warp shaders lost a few slots with the clamp
  (`inter_warp` 885 → 882, `inter_warp_hbd` 995 → 994).
- **Shader cache of the workflow** ([test-player.md](test-player.md#shader-cache-of-the-workflow)):
  one FXC call per shader and variant, kept by content, instead of five compiles of every
  shader per run.

**The scaled-reference stream** (`syn_resize-mode`, SVT-AV1 `resize-mode=2`, 10-bit) stood here as
wrong on the console on 53 of 64 frames and right on WARP. *Correction, 2026-11-12: the console decodes
it correctly.* The md5 references the console's output was compared with had been made with ffmpeg's
`-f framemd5` without `-noautoscale`, and ffmpeg then scales every frame to the size of the first one:
the reference of a frame of another size was the hash of a scaled picture. The conformance run on WARP
never had that problem (`make_md5_ref.sh` passes `-noautoscale`). Against dav1d at each frame's own size,
every hash the console had logged as a mismatch is the right one (52 of 52 and 53 of 53 in the two
logs). An independent review of the references found it. What the stream runs, and so what is right on
the console: frames of nine sizes from 960x540 to 1920x1080, single-reference and averaged compound
prediction from scaled references (`inter_scale`, `inter_scale_compound`, the 2x2 chroma shaders), 31
hidden frames shown later; no warped block, no OBMC, no wedge and no difference weighted compound
(`allow_warped_motion` 0, `is_motion_mode_switchable` 0, no global motion model,
`enable_masked_compound` 0, `enable_jnt_comp` 0, `enable_interintra_compound` 0).

Not run on the console: everything in this section.

## Console-only mismatches located by stage dumps (2026-11-12)

*Correction, 2026-11-12:* this section was written with two console-only mismatches in mind. There is
one, the 8-bit loop filter ([its cause](#a-paired-read-before-an-overlapping-in-place-write-2026-11-12)). The other, a 10-bit stream with scaled references, was an
error in the md5 references it was compared with (ffmpeg scaled every frame to the size of the first;
[above](#layers-new-sequences-mixed-reference-sizes-all-conformance-streams-pass-on-warp-2026-11-12)):
the console decodes that stream correctly.

The player and warp_decoder save the frame after every GPU stage (`-dumpstages`,
`testing/stage_dump/stage_dump.py compare`). Console dumps (package 0.2.25.0) against CI's WARP dumps
of the same streams:

- **8-bit**: AOM's tiny vector `av1-1-b8-01-size-66x66` reproduces the 8-bit bug on the console (frame 1
  of 2 wrong; right on WARP). Every stage up to and including the even pass of the vertical loop filter
  is byte-identical, border and padding included. The odd pass (`lf_vert`, the second column of 64x64
  blocks) is the first stage that differs: 4 luma samples by 1, at x 64..65 (the last two columns of
  the frame), y 41, 52, 61. `loopfilter_vert.hlsl` filters one row per thread with nothing shared
  between threads, and its frame input is identical, so the suspect is its other input, the edge list
  (`lf_blocks`, written by `gen_lf_blocks_vert`), which the dumps do not cover yet — for example
  entries at the right frame edge that are not written and hold whatever the buffer held (the console
  does not have to zero a new buffer; WARP's are fresh).
- **Scaled references**: decoded frames 0-3 of CI's `syn-resize-10b-1080p` (three of them predicted
  from references of another size) are identical on every stage, and so is everything after them: the
  "first wrong output" at decoded frame 4 (1280x720, the first shown frame of another size than the
  first) was the first frame whose md5 reference was of a scaled picture.
- **None of the console experiments changed the 8-bit output**: `-lfhsplit 1|2`, `-intraedgefix`, the
  uavsync shaders and the bit-field table masks give the same wrong md5 (8-bit 4K frame 0 `537ee12a...`
  in every build since 0.2.11.0). Super-resolution, fixed on WARP, is md5-identical on the console too.

### 8-bit: wrong exactly where the loop filter runs

Frame headers of AOM's vectors (`ffmpeg -bsf:v trace_headers`) against the console results of package
0.2.29.0:

| Stream (8-bit) | Console | `loop_filter_level[0]`, `[1]` (vertical, horizontal luma edges) |
| --- | --- | --- |
| `av1-1-b8-22-svc-L2T1`, `-L2T2` | all frames right | 0, 0 in every frame of both layers: no frame is loop filtered (CDEF is on) |
| `av1-1-b8-01-size-66x66` | frame 0 right, frame 1 wrong | frame 0: 0, 0; frame 1: 4, 5 (U 0, V 4) |
| `av1-1-b8-03-sizeup`, `-sizedown` | all frames wrong, both sizes | key frame 352x288: 5, 0 (U 3, V 2); key frame 1280x720: 38, 26 (U 20, V 14); the frames without loop filter predict from wrong references |
| 8-bit 2160p cut | all frames wrong | frame 0, a key frame: level 1 |

All of these have `loop_filter_delta_enabled` 1 with the default reference deltas, no segmentation and
no `delta_lf`; sharpness 0. So on the console an 8-bit frame is wrong when, and as far as these streams
tell only when, the loop filter runs on it, and the 10-bit streams with loop filter are right. The
list shaders (`gen_lf_blocks_vert/hor`) are the same for both depths; the filters are not
(`loopfilter_vert/hor.hlsl` against the `_hbd` files: four samples to a 32-bit word instead of two,
whole words read and written back around an edge). The vectors differ from the 10-bit streams in one
more way: their vertical and horizontal levels differ (one of them can be 0), and their level varies
with the reference of the block.

### 66x66 frame 1: the four samples follow from one term of the 8-tap filter

WARP's dump of frame 1 with the new buffers (run 43160403800) says what the odd pass does there. The
left 64 columns are one 64x64 block with one transform, so the even pass filters nothing in luma
(`lf_vert_even` equals `intra`), and the odd pass is one edge per row, at x 64, level 4, 8 taps (the
block right of it is 8x16 with an 8x16 transform; `stage_dump.py edges ... --x 64 --y 41`). Samples x 66
to 71 of these rows are outside the picture but inside the block, and the 8-tap filter reads x 60..67.

A model of `filter_vertical_edge` of `loopfilter_vert.hlsl` in Python reproduces WARP's `lf_vert` from
its `lf_vert_even` in all 68 rows. The filter applies its 8-tap branch in 14 of them. Changing the model
one thing at a time (another level, another filter length, another `hev` threshold, any one input sample
of x 56..71 off by up to 3, an operand of one of the three q-side steps replaced by another sample) and
comparing with WARP gives the console's picture, four luma samples at x 64..65 in rows 41, 52 and 61 and
nothing else, for one family only: the first q-side step of the 8-tap branch, `v += -p1.x + q0.w`, adds
about nothing instead of `q0.w - p1.x`. The values that predicts for the console's `lf_vert`:

| x, y | WARP | predicted for the console |
| --- | --- | --- |
| 65, 41 | 153 | 152 |
| 65, 52 | 153 | 154 |
| 64, 61 | 166 | 167 |
| 65, 61 | 166 | 167 |

with x 66 and 67 of every row as on WARP. In FXC's listing of the shader (`shader-survey/asm/
loopfilter_vert.asm`) that step is `iadd r2.x, r3.w, -r3.y` (`r3.w` is `q0.w`, `r3.y` is `p1.x`), and
four instructions further down the same two components are rewritten in place by one vector
instruction, `iadd r3.yw, r3.wwww, -r4.xxxy` (`q0.w - p1.y` and `q0.w - p1.z`, the next two steps). If
the first instruction reads `r3.w` and `r3.y` as they are after the second, it adds `p1.y - p1.z`, which
is 0 or 1 where the 8-tap branch runs, and that gives exactly the four values of the table. The
listing of `loopfilter_vert_hbd` has no instruction that writes components of a register from other
written components of the same register; `loopfilter_vert` has five, `loopfilter_hor` three,
`loopfilter_hor_hbd` two (so not every such instruction goes wrong there).

This is a reading of WARP's data against the console's reported differences, not a console result. It
is checked without a new run when the console's four samples have the values of the table. If they do,
the edge lists and the frame are not what differs: the console runs that one sequence of the 8-bit
shader differently, every 8-bit frame with an 8-tap vertical edge in a flat place is affected, and the
candidate fix is to write the 8-tap branch of the 8-bit filters so that FXC does not produce that
sequence (each output from its own sum, as the 14-tap branch does), with the listing checked for it.

**Checked, 2026-11-12.** The console's dump (package 0.2.25.0) has exactly the four values of the
table: 152, 154, 167, 167. The step is the right one and so is the instruction. What that instruction
read is not what is written above: fourteen edges cannot tell `p1.y - p1.z` from other small values,
and a 3840x2160 frame can. The console ran `iadd r2.x, r3.w, -r3.y` as `r3.z - r3.x`
([below](#a-paired-read-before-an-overlapping-in-place-write-2026-11-12)), which gives the same four values here.

### The edge lists: read through, nothing unwritten

What the loop filter reads of `loopfilter_blocks`, and what writes it:

- The buffer is created once (`av1_core.cpp:656-667`), is shared by all frames and slots and is never
  cleared. It does not have to be: every thread of a list shader first zeroes the 32 bytes of its own
  block and then stores the block's edges (`gen_lf_blocks_vert.hlsl:146-147` and `:231`,
  `gen_lf_blocks_hor.hlsl:146-147` and `:224`), so a record holds nothing of an earlier frame, and a
  block that ends early (no mode info, `:162`, `:189`; an edge that is not on the transform grid,
  `:178`) stays zero, which the filter takes as "no edge".
- The list shaders write exactly the records the filters read. Vertical: `blk_cols * mi_rows` blocks
  per plane (`loopfilters.cpp:92-102`, dispatched with that count at `:156`), read as the even block
  numbers and then the odd ones (`:229`, `:243`), together all of them. Horizontal: `mi_cols *
  ceil(mi_rows / 16)` blocks (`:172`), read at `:264`. A thread behind the count returns
  (`gen_lf_blocks_*.hlsl:131`) or is inactive in the filter. A plane with level 0 is skipped by both.
- Nothing outside the frame is read. A block at the right edge has `c_end = clamp(mi_cols - mi_col, 0,
  16)` edges (`gen_lf_blocks_vert.hlsl:144`; `r_end` at the bottom, `gen_lf_blocks_hor.hlsl:143`), with
  `mi_cols` the 4x4 columns that have a visible sample (`loopfilters.cpp:87-90`), and reads the mode
  info of those columns and of the one to the left only; the entries behind the last edge are the
  zeroes. For 66x66 the second block of a row has the one edge at x 64.
- The filter walks a block from entry 0 by the step of each edge and stops at a zero or at sixteen
  (`loopfilter_vert.hlsl:199-211`, `loopfilter_hor.hlsl:246-255`), so it reads only entries a list
  shader wrote in this frame.

Three places of the list shaders index a table with a value of the mode info without a bound
(`tx_size_wide[tx_size]`, `gen_lf_blocks_vert.hlsl:121`; the block size tables, `:170-171`; the level
table by reference and mode, `:95`); they are in range for every record a stream can produce, so they
are not the difference either, and nothing was changed. `-dumpstages` now saves the lists, the
constants and the mode info (format 2, [test-player.md](test-player.md#stage-dumps--dumpstages)), so
that a console dump says which of the three inputs of `lf_vert` differs, if any.

## A paired read before an overlapping in-place write (2026-11-12)

The 8-bit streams are wrong on the console because its driver, which compiles the shaders' DXBC again
for its GPU, compiles one instruction of the 8-bit vertical loop filter wrongly. WARP runs the same
bytecode as written.

### What the console computes

Frame 0 of the 8-bit 2160p cut, a key frame with loop filter level 1: the console's dump (package
0.2.25.0) has the frame before the loop filter (`intra`), after the vertical edges (`lf_vert`) and after
the horizontal ones (`lf_horz`); the digest of WARP's dump of the same frame (run 43160403800) has the
filter's constants and edge lists as they are, and a hash of every row of every stage. The two agree in
`idct` and `intra`, so the lists, which the list shaders make from the mode info, are the console's too.
`testing/stage_dump/lf_model.py` filters a dump's own `intra` with the arithmetic of the two shaders:

```
lf_model.py console/<8-bit 2160p>_f0.stages --lists warp/<8-bit 2160p>_f0.digest
  vertical edges, Y: model of intra against lf_vert: 31769 of 8294400 samples differ; against the row hashes of the digest: 0 of 2160 rows differ
      edges of a row or column that were filtered: 14-tap 47970, 4-tap 52495, 8-tap 40240
  vertical edges, U: ... 0 of 2073600 samples differ; ... 0 of 1080 rows differ        (V the same)
  horizontal edges, Y: model of lf_vert against lf_horz: 0 of 8294400 samples differ
      edges of a row or column that were filtered: 14-tap 34484, 4-tap 45225, 8-tap 31814
  horizontal edges, U: ... 0 of 2073600 samples differ                                   (V the same)
```

So the model is the shader (it gives WARP's rows, all of them), the console's horizontal filter and its
chroma are as written, and its vertical luma filter is not. Edge by edge, the console differs from the
model in the 8-tap branch only (20656 of its 40240 edges), and there only in the three samples right
of the edge (x, x + 1, x + 2 of an edge at x: 11129, 10174 and 10466 samples); the 47970 edges of the
14-tap branch and the 52495 of the 4-tap branch are right, and so is every decision which branch an
edge takes. The 8-tap branch keeps a running sum `v`; the sample at x is `(v + q0.x + 4) >> 3` after the
step `v += -p1.x + q0.w`. Of all differences of two of the eight samples around the edge, one in place
of `q0.w - p1.x` gives the console's sample at x on all 40240 edges: `p1.w - q0.x`. With that one change
(`--variant console-0.2.25`) the model gives the console's whole `lf_vert`:

```
  vertical edges, Y: model of intra against lf_vert: 0 of 8294400 samples differ
```

The reading of the 66x66 section above (the step adds `p1.y - p1.z`, the two components read after they
were rewritten) gives the four samples of the 66x66 frame as well, and leaves 16771 samples of this
frame unexplained; `p1.w - q0.x` gives both frames.

### The instructions

FXC's listing of the branch (`loopfilter_vert.asm` of run 43160403800; `r3` holds `q0.x`, `p1.x`,
`p1.w`, `q0.w` in x, y, z, w; `r4` holds `p1.y`, `p1.z`, `q0.y`, `q0.z`; `r1.w` is `v`):

```
353  iadd r2.x, r3.w, -r3.y            ; q0.w - p1.x
354  iadd r1.w, r1.w, r2.x             ; v += ...
355  iadd r2.x, r3.x, r1.w             ; v + q0.x
356  iadd r2.x, r2.x, l(4)
357  iadd r3.yw, r3.wwww, -r4.xxxy     ; r3.y = q0.w - p1.y, r3.w = q0.w - p1.z: the next two steps
358  iadd r1.w, r1.w, r3.y
```

The console ran instruction 353 as `r3.z - r3.x`: each of the two components replaced by the one before
it. Instruction 357 writes those two components of `r3`, and computes both from one of them. Nothing
else about 353 is unusual, and the same shader shows what is not the cause, because the console runs it
correctly:

- an instruction that reads two components of one register into one result, one of them negated:
  `iadd r5.w, -r3.z, r3.y`, `iadd r6.y, -r3.x, r3.w` and others in the flatness tests, which decide the
  branch of every edge;
- a vector instruction that rewrites components of a register from the same register across components
  (an *overlapping in-place write*, below): `ige r5.yz, cb0[r1.x + 3].xxxx, r5.xxyx` and two more in the
  mask, `iadd r3.xy, r3.xxxx, l(4, 3, 0, 0)` in the 4-tap branch, after `imad r3.x, l(3), r3.y, r3.x`
  had read both components and rewritten one;
- in the horizontal filter even the pair of the two, `imad r12.z, r13.y, l(3), r13.z` and 26
  instructions later `iadd r13.yz, r13.zzwz, r13.yyzy`, in the 14-tap branch, which is right on all
  34484 edges.

What the driver does inside is not known. What is known is one sequence it gets wrong and that the
sequences next to it are right, so the rule below is drawn around the one, widely enough to hold
everything that looks like it, and is not a statement that every hit is wrong.

### The pattern and the scan

`testing/dxbc_scan/scan.py` reads the listings (every instruction; an opcode it does not know stops it):

- **Overlapping in-place write**: an instruction that writes components of a temp register and reads,
  of the same register, for at least one written component, another component that it also writes
  (`r3.y` from `r3.w` in 357). These are the instructions whose result depends on the order in which
  components are read and written. FXC emits them wherever a vector result fits over values that end
  there: 1649 in 87 of the 92 shaders, most of them in paths that are md5-identical on the console.
  Counted in the report, not refused.
- **Paired read before it**: such a write by an arithmetic or logic instruction, and before it, with
  no flow control instruction between the two, an instruction that does not write the register and
  reads two or more of the components the write replaces into one component of its result, while they
  still hold the values the write replaces (353 and 357). **Refused.** With a load as the write (the
  address is a component the load replaces) it is listed and not refused: 6 before, 5 now.

Before the change the scan found ten pairs:

| Shader, line | The write | The read before it | Console | What was done |
| --- | --- | --- | --- | --- |
| `loopfilter_vert` 357 | `iadd r3.yw, r3.wwww, -r4.xxxy` | 353 `iadd r2.x, r3.w, -r3.y` | **wrong** | every output of the 8-tap branch from its own sum |
| `loopfilter_hor` 360 | `iadd r13.yz, r13.zzwz, r13.yyzy` | 334 `imad r12.z, r13.y, l(3), r13.z` | right (model, above) | one sum of the 14-tap branch in another order |
| `inter_warp` 706, `inter_warp_hbd` 802 | `imad r1.yz, cb0[r1.y + 3].xxxx, r4.yyzy, r1.zzzz` | the same `imad` of the rows before | 10-bit: right (md5, global motion clips) | the eleven row addresses as vectors of 4, 3 and 4 |
| `inter_compound_masked_hbd` 880 | `iadd r4.xyzw, r4.zzzz, l(2, 3, 4, 5)` | 866 `imad r3.x, cb0[r3.w + 28].w, r4.w, r4.x` | not known | the row addresses of the second reference as vectors |
| `inter_compound_masked_hbd` 1182 | `movc r0.xyzw, r0.wwww, r2.xyzw, r4.xyzw` | 1178 `iadd r3.w, r0.z, r0.x` | not known | the residual is added in a branch (`[branch]`), not selected |
| `inter_2x2chroma_hbd` 455 | `movc r1.xyzw, r1.wwww, r3.xyzw, r2.xyzw` | 441 `iadd r0.yz, r1.yyyy, cb0[r1.z + 0].wwyw` | not known | the same |
| `idct4x16` 331, `idct32x16` 361 | `iadd r3.xy, -r3.yzyy, r3.xwxx` | `iadd r9.zw, r3.zzzy, r3.wwwx` (`r24.xy`) | right (md5) | allow-list |
| `idct64x16` 515 | `iadd r2.yz, -r2.zzwz, r2.yyyy` | 512 `iadd r41.xy, r2.wzww, r2.yyyy` | right (md5) | allow-list |

The three on the allow-list (`testing/dxbc_scan/allow.txt`) are one butterfly of the 16-point inverse
DCT, two sums and then the two differences over two of the four inputs. FXC's output for it does not
depend on the order of the statements (four orders tried), and the same butterfly is not a pair in the
other shaders with that transform. They are accepted because the console runs them correctly: every
column of a block with one of these transforms and a DCT in the columns executes the pair, a wrong
difference is a wrong residual, and the console is md5-identical to dav1d (packages 0.2.19.0 and
0.2.29.0; the IDCT shaders have not changed since `c35d97f`) on 10-bit streams whose key frames have,
with coefficients, 32x16 transforms in five of the streams (7 to 37 in the key frame), 4x16 in two (8
and 5) and 64x16 in one (178), counted with libaom's inspector. An allow-list entry that matches no
pair is an error, so a change to these shaders brings the question back.

After the change: 1651 overlapping in-place writes in 87 shaders, three pairs, all on the allow-list,
none refused; the same in the uavsync listings.

### The change

`loopfilter_vert.hlsl`, 8-tap branch: the six outputs each from its own sum, as the 14-tap branch has
them, instead of from the running sum. The sums are those the running sum had at each output, so the
arithmetic is the same:

```
x-3: (3 p1.x + 2 p1.y + p1.z + p1.w + q0.x + 4) >> 3        x:   (p1.y + p1.z + p1.w + 2 q0.x + q0.y + q0.z + q0.w + 4) >> 3
x-2: (2 p1.x + p1.y + 2 p1.z + p1.w + q0.x + q0.y + 4) >> 3  x+1: (p1.z + p1.w + q0.x + 2 q0.y + q0.z + 2 q0.w + 4) >> 3
x-1: (p1.x + p1.y + p1.z + 2 p1.w + q0.x + q0.y + q0.z + 4) >> 3   x+2: (p1.w + q0.x + q0.y + 2 q0.z + 3 q0.w + 4) >> 3
```

The listing of the q side now (402 → 414 instruction slots, 11 temps as before); no instruction of the
branch writes a register from another component of itself:

```
360  iadd r1.w, r4.y, r4.x             ; p1.y + p1.z
361  iadd r1.w, r3.z, r1.w             ; + p1.w
362  iadd r1.w, r2.w, r1.w             ; + 2 q0.x
363  iadd r1.w, r4.z, r1.w             ; + q0.y
364  iadd r1.w, r4.w, r1.w             ; + q0.z
365  iadd r1.w, r3.w, r1.w             ; + q0.w
366  iadd r1.w, r1.w, l(4)
367  ishr r1.w, r1.w, l(3)             ; the sample at x
368  iadd r2.x, r3.z, r4.y             ; p1.z + p1.w ...
...
```

The other five changes are there for the scan only; the console is not known to run any of them
wrongly, and two of them it runs correctly. Slots before → after: `loopfilter_hor` 577 → 578,
`inter_warp` 882 → 882, `inter_warp_hbd` 994 → 988, `inter_compound_masked_hbd` 1096 → 1122,
`inter_2x2chroma_hbd` 387 → 393. The horizontal 8-bit filter and the two 10-bit filters keep their
running sums: no pair there, and right on the console.

On WARP the output is the same: all conformance streams are md5-identical to their references with
the changed shaders, and the host checks pass (the run of the commit that made this change).

### The check

The workflow scans the listings of both variants after the survey (`Listings scan` step:
`scan.py --self-test`, which must find the pair above in a copy of the old listing and must not find
it in five sequences next to it, then the scan). A pair that is not on the allow-list stops the run;
`shader-survey/listing-scan.txt` in the artifact has every overlapping in-place write, per shader, and
the pairs. See [test-player.md](test-player.md#listings-scan).

### Not run on the console

The change itself. Expected there: the 8-bit streams md5-identical to dav1d (the 2160p cut, the 720p
Chimera cut, `av1-1-b8-03-sizeup` and `-sizedown`, `av1-1-b8-01-size-66x66`), the 10-bit regression set
unchanged. If an 8-bit stream is still wrong, `-dumpstages` of a wrong frame and `lf_model.py` on the
dump say whether a loop filter stage is still not as written.

**Console result (package 0.2.31.0, rav1g 709cf04).** md5-identical to dav1d: the 8-bit 4K, 720p and 1080p
cuts, `av1-1-b8-03-sizeup` and `-sizedown` (with native-size references), `av1-1-b8-01-size-66x66`, the
SVC vector, both resize streams, the superres stream, and the 10-bit set (4K low-delay and random access,
Jellyfin 4K, 1080p and 4K film grain with global motion, 1200 frames of Sparks). Frame times unchanged
within noise (`-gputime`, against package 0.2.25.0: GPU busy 30.3 vs 31.0 ms, 51.2 vs 51.1 ms and 35.7 vs
35.8 ms on three 4K clips; the vertical loop filter 0.81 vs 0.83 ms). With this, no stream we test differs
from dav1d on the console or on WARP.

## Shader Model 6 feasibility (2026-11-12)

All shaders are FXC shaders of Shader Model 5.0. Moving them to DXC and Shader Model 6 would give wave
intrinsics (a sum or a prefix sum over the lanes of a wave without group memory and group syncs) and
native 16-bit types (samples and coefficients as `uint16_t` and `int16_t` instead of 32-bit words). Whether that is open depends on the
console: what its D3D12 takes in app mode, and how wide its waves are.

Nothing of the decoder was changed to find out. Both players log what the device reports at every
launch (`CAPS` lines: shader model, `WaveOps` with the lane counts, `Native16BitShaderOpsSupported`,
and more), and with `-sm6test` they run 14 DXC test shaders, each on known input and compared with
values computed on the host (`SM6TEST` lines): a `cs_6_0` control, the wave intrinsics, a 64-element
sum and prefix sum through group memory and through the wave, `uint16_t`/`int16_t`/`float16_t`
arithmetic and 16-bit loads and stores (`cs_6_2`), `[WaveSize(n)]` for 4 to 64 lanes (`cs_6_6`) and a
range of wave sizes (`cs_6_8`). A pipeline state the device refuses is a result. See
[test-player.md](test-player.md#shader-model-6-test--sm6test).

The questions the console run answers: does the device take DXIL; is a wave 32 or 64 lanes, and is a
thread group of 64 one wave; do the wave results match the group memory path; do 16-bit types work
and wrap at 16 bits; can a shader ask for a wave size.

**WARP, for reference** (run 43179921793): shader model 6.8, `WaveOps` with 4 lanes (minimum and
maximum), native 16-bit operations reported. 9 of the 14 tests pass: the control, the wave intrinsics
(64 waves of 4 consecutive threads in the 256 threads of the dispatch), both reductions with the same
sums, the 16-bit arithmetic and the 16-bit loads and stores, `[WaveSize(4)]`, and both range forms,
which get 4 lanes whatever they prefer. The other 5 are the pipeline states with `[WaveSize]` of 8, 16,
32 and 64, which WARP refuses (`0x80070057`): it has waves of 4 lanes only. Nothing computed a wrong
value.

**Console result** (package 0.2.34.0, rav1g e52e07a, single task line `-sm6test`): the device reports
**shader model 6.4**, feature level 11_0, `WaveOps` with waves of **64 lanes, minimum and maximum**,
`Native16BitShaderOpsSupported` **0**, no `[WaveSize]` (it needs 6.6), typed UAV loads only for 32-bit
formats. DXIL signed by DXC 1.9 is accepted.

| Test | Console |
| --- | --- |
| `basic` (cs_6_0 control) | PASS |
| `wave_ops` | lane count 64; 4 waves of 64 consecutive threads in 4 groups of 64, no wave across groups; sum, prefix sum, ballot, first-lane reads right; `WaveReadLaneAt(lane ^ 1)` FAIL (188 mismatches, every thread got the same value) |
| `reduce_groupshared`, `reduce_wave` | PASS, the same sums and prefix sums |
| `native16_math`, `native16_load` (cs_6_2, `-enable-16bit-types`) | pipeline state refused (`0x80070057`) |
| `wavesize*`, `reduce_wave64`, `wavesize_range_*` (cs_6_6/6_8) | pipeline state refused (`0x80070057`) |

**Reading:**
- **Wave intrinsics are available** with DXC up to `cs_6_4`, on waves of 64 lanes. A thread group of 64
  is one wave on the console, which is why upstream's lockstep code worked there.
- **The one failure is the test's, not the console's:** in Shader Model 6.0 `WaveReadLaneAt` needs a lane
  index that is the same for the whole wave; a per-thread index is undefined, and the console reads one
  lane for all. A per-lane exchange needs the quad operations or group memory instead.
- **No native 16-bit arithmetic** and no requested wave size: 8- and 10-bit pixel maths stays in 32-bit
  registers, and shaders can rely on waves of 64 (and should still not assume them).
- So a move to DXC/SM 6.0-6.4 would bring wave reductions, broadcasts and ballots (useful for the CFL and
  DC sums, the CDEF direction search, compaction of block lists) but not packed 16-bit throughput.

## Inter prediction: breakdown and fast paths (2026-11-12)

### What the console measured

Package 0.2.36.0, `-gputimedetail` and `-gputimeisolate`
([test-player.md](test-player.md#inter-prediction-breakdown--gputimedetail--gputimeisolate)), 4K 10-bit, 300 frames
per clip. Times are per frame.

| Clip | GPU busy | inter | inter by section | fetches without a fraction | x only | y only | both | reference bytes loaded per byte written |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
| SVT low-delay, 40 Mbps | 32 ms | 7.4 ms | single 100% | 67.9% | 14.6% | 14.2% | 3.3% | 8.2x |
| SVT random access, 12.6 Mbps | 35 ms | 11.2 ms | single 47%, comp_avg 52% | 68.0% | 13.7% | 13.5% | 4.9% | 19.8x |
| Jellyfin, 40 Mbps | 49 ms | 17.3 ms | single 94% | 60.4% | 19.4% | 17.2% | 3.0% | 8.5x |
| Chimera with film grain (libaom, 8 tiles) | 68 ms | 47.1 ms | comp_avg 40%, difference weighted 22% (comp_diff_y 12.8 + comp_diff_uv 9.3), single 23%, OBMC 10% | 38.1% | 10.4% | 16.9% | 34.7% | 16.8x |

- Every stream asks for the 8-tap (regular) kernels for 100% of its area. The fractions are those of
  the motion vector of a reference fetch, by luma area.
- A sample of a small block costs 5 to 9 times what a sample of a large block costs (Jellyfin clip,
  `-gputimeisolate`: 4x4 10.3 ns, 8x8 9.9, 16x16 6.9, 32x32 2.3, 64x64 1.2 ns per sample).
- The shaders load 75 to 400 MB of reference per frame for 8 to 24 MB written, at only a few GB/s. The
  stage is bound by the latency and the count of its loads and by the arithmetic behind them, not by
  memory bandwidth.
- 24 fps needs at most 41.7 ms of GPU per frame. Three of the four clips are near or over it with inter
  as a large part.

### What the original shaders do with every block

`inter_main` (one reference; a thread predicts 4x4 samples) loads 11 rows of 11 samples around its 4x4
and runs both 8-tap passes: `filter_line()` along each of the 11 rows, then a vertical sum of 8 rows per
output row. `inter_compound` (two references; a thread predicts 4x2) does the same with 9 rows of each
reference and keeps four more bits per reference for the blend. Neither looks at the motion vector: a
vector without a fraction selects the kernel `{0, 0, 0, 128, 0, 0, 0, 0}`, which every filter type has at
fraction 0, and the two passes then multiply the sample by 128 twice and round it back. Two thirds of
the fetches of three of the clips are of that kind.

### The fast path (`-interfast`)

Off by default. `av1_set_inter_fast()` and `aom_codec_dec_cfg_t::inter_fast` in the library, `-interfast`
in the test player and in `warp_decoder`; `-interfastlists` keeps the original shaders on the new lists, to
tell what the lists do from what the kernels do. Without the flag the block lists, the dispatches and the
pipeline states that run are the ones that always ran (five more pipeline states are created at
launch).

**Classes.** A reference fetch has a class by the fraction of its motion vector, in the plane it is for
(1/8 sample for luma, 1/16 for chroma, so the chroma of a block can have another class than its luma):

| Class | Fraction | What it needs |
| --- | --- | --- |
| 0 `copy` | none | the samples |
| 1 `h` | in x only | one 8-tap pass along the row |
| 2 `v` | in y only | one 8-tap pass down the column |
| 3 `2d` | both | the two passes of the original shaders |

**Lists.** The classes are separated where the block lists are built, not inside a shader. A group of
64 threads is one wave on the console and runs in lockstep, so a branch that the threads of a group take
differently costs the group every side of it; with 4x4 and 8x8 blocks a group holds 16 to 64 blocks, and
those are the expensive ones. On a frame that takes the path, the blocks that would go to the lists
`CasualInter` (one reference, not warped) and `CompoundAvrg` (two references, averaged or distance
weighted) are counted by the tile threads (`av1_mi_push_block`) into lists by class and block size
instead: 4 classes times 24 sizes for one reference, 16 pairs of classes times 24 sizes for two. The lists
live in a block map of their own per tile behind the existing ones (1984 bytes per tile the decoder size
can have, one per 64x64, and pipeline slot: 1.0 MiB per slot at 1080p, 3.9 MiB at 4K; only in a decoder
created for the path), `av1_prediction_gen_blocks` gives them their place in
`prediction_blocks` behind the lists by type, and `gen_pred_blocks_fast.hlsl` (`gen_pred_blocks.hlsl`
built with `INTER_FAST`: one more root constant, the class from the record's vector) writes the same
16-byte records into them. `gen_pred_blocks.hlsl` itself compiles to the bytecode it had.

**Dispatch.** One dispatch per class (or pair) and block size, in pass 1 behind `comp_masked`. The class
goes to the shader in bits 8 and up of the `width_log2` root constant, so the choice of path is the same
for every thread of a dispatch: three `if` in the listing of `inter_fast`, three per reference in
`inter_compound_fast`, taken the same way by the whole wave. The list of class 3 runs `inter_main`, the
list of the pair (3, 3) runs `inter_compound`.

**Not on the path**, and exactly as before: frames with a scaled reference (all their blocks go through
`inter_scale_*`), warped blocks, wedge and difference weighted compound blocks, OBMC, and the chroma of
blocks under 8x8 (`inter_2x2chroma`).

### The kernels and why they are exact

`p` is a reference sample, `S` a plain sum of 8 taps times samples (the taps sum to 128), `b` is 14 for
8-bit and 16 for 10-bit frames.

*One reference.* `filter_line()` returns `l = (S + 4 + 2^b) >> 3 = ((S + 4) >> 3) + 2^(b-3)`; for
fraction 0, `S = 128 p` and `l = 16 p + 2^(b-3)` exactly. `inter_main` then forms `V = sum of tap * l`
over 8 rows and outputs `clamp(((V + 2^(b+5) + 1024) >> 11) - 2^(b-6) - 2^(b-7))` (8-bit: the `2^19` its
sums start from; 10-bit: the `2^21` in its rounding term).

| Class | What `inter_main` computes | `inter_fast` |
| --- | --- | --- |
| copy | `l = 16 p + 2^(b-3)`, `V = 128 l`: the brackets come to `p + 2^(b-6) + 2^(b-7)`, so the output is `p` | `p` |
| h | `V = 128 l`: `((l + 8) >> 4) - 2^(b-7)`, which is `(((S + 4) >> 3) + 8) >> 4` | `filter_line()` unchanged, then `(l + 8 - 2^(b-3)) >> 4` |
| v | every `l` is `16 p + 2^(b-3)`, so `V = 16 S + 128 * 2^(b-3)` and the output is `(S + 64) >> 7` | `(S + 64) >> 7` |

These are libaom's `av1_convolve_2d_copy_sr`, `av1_convolve_x_sr` (`round_0 = 3`, then 4 more bits) and
`av1_convolve_y_sr` (`FILTER_BITS = 7`). All shifts are arithmetic and no term overflows 32 bits (a tap
is under 2^8 in magnitude, a sample under 2^16).

*Two references.* `inter_compound` blends `src = (V + 64 + 2^(b+5)) >> 7` of each reference:
`(((src0 * w0 + src1 * w1) >> 4) - OutputSub + 8) >> 4`, with `OutputSub = 2^(b-2) + 2^(b-3)` (6144 and
24576). `inter_compound_fast` blends with the same expression and takes `src` per reference from its
class:

| Class | `src` |
| --- | --- |
| copy | `16 p + OutputSub` |
| h | `((S + 4) >> 3) + OutputSub`, as `l + OutputSub - 2^(b-3)` from `filter_line()` |
| v | `((S + 4) >> 3) + OutputSub` (the vertical sum is `16 S + 128 * 2^(b-3)`; `(16 S + 64) >> 7 = (S + 4) >> 3`) |
| 2d | `(V + 64 + 2^(b+5)) >> 7`, the original expression |

The positions are those of the original shaders: the same clamp of the first column to `[-11, width - 1]`
and of every row to `[0, height - 1]`; a pass that is left out reads the sample under tap 3 of the pass
that would have run.

**What a thread loads** (bytes of one row times rows; a row of 4 samples is one load of 8 bytes, or two
loads of 8 bytes for 16-bit samples, a row of 11 samples is 16 or 24 bytes as in `filter_line()`):

| | original | copy | h | v |
| --- | --- | --- | --- | --- |
| one reference, 4x4, 10-bit | 11 x 24 = 264 | 4 x 16 = 64 | 4 x 24 = 96 | 11 x 16 = 176 |
| one reference, 4x4, 8-bit | 11 x 16 = 176 | 4 x 8 = 32 | 4 x 16 = 64 | 11 x 8 = 88 |
| per reference of a compound 4x2, 10-bit | 9 x 24 = 216 | 2 x 16 = 32 | 2 x 24 = 48 | 9 x 16 = 144 |
| per reference of a compound 4x2, 8-bit | 9 x 16 = 144 | 2 x 8 = 16 | 2 x 16 = 32 | 9 x 8 = 72 |

`copy` and `h` load the rows they write and no others; `v` keeps its rows and loads them narrower.

**Shaders** (FXC, `cs_5_0`; instruction slots and temp registers of the listings):

| Shader | Slots | Temps | For comparison |
| --- | --- | --- | --- |
| `inter_fast` | 676 | 20 | `inter_main` 691, 14 |
| `inter_fast_hbd` | 625 | 23 | `inter_main_hbd` 707, 14 |
| `inter_compound_fast` | 1729 | 22 | `inter_compound` 1016, 15 |
| `inter_compound_fast_hbd` | 1743 | 28 | `inter_compound_hbd` 1041, 16 |
| `gen_pred_blocks_fast` | 1557 | 23 | `gen_pred_blocks` 1522, 23 |

The counts are static: a listing of a fast shader has every class, a thread runs one. No group memory,
no group sync, no loop. The 16-bit row of four samples is put together from two loads one word apart,
because shifting three loaded words in place compiled to the pair of instructions the listings scan
refuses ([A paired read before an overlapping in-place write](#a-paired-read-before-an-overlapping-in-place-write-2026-11-12));
the scan passes on all five.

### Shared loading: not done

The other idea, loading the footprint of a block of 16x16 or more once per thread group into group
memory and filtering from there, is not in this prototype:

- With the classes, `copy` and `h` have nothing left to share between rows: a thread loads only the rows
  it writes. What neighbours still load twice is in `v` and `2d` (7 of a thread's 11 rows are also its
  vertical neighbour's) and the 7 of 11 samples two `h` threads side by side have in common.
- It is not a variant of these shaders. A group has to hold whole blocks and their margins in group
  memory (a 16x16 block with margins is 23x23 samples, four of them per group; a 64x64 block spans four
  groups), with a group sync between loading and filtering and a thread layout per block size.
- Whether it is worth that is what the per-class times of the console run say: `single.v`, `single.2d`
  and the `comp_avg` pairs with a `v` or a `2` by block size (`-gputimeisolate`).

Small blocks are already predicted many per group (64 blocks of 4x4, in decode order, so neighbours
share a group); several blocks per group gain nothing further without group memory.

### Proof of equality

- **Host check** (`testing/inter_check`, a job of the workflow): the four fast shaders and the four
  original ones compiled as C++, generated frames laid out like the frame pool, records sorted into lists
  with `inter_fast_class()` taken from `gen_pred_blocks.hlsl`. Every frame is predicted by libaom's
  `av1_make_inter_predictor`, by the original shaders, and the way the fast path dispatches it; the three
  pools must be byte identical. Covered, and checked to be covered: all 22 block sizes with every class
  (luma and chroma), every pair of classes for two references, all 16x16 fractions of a vector with all
  16 pairs of the four filters, distance weights, residuals, samples at both ends of the range, vectors
  up to 168 samples outside the frame on every side, 8-bit and 10-bit; 232 frames, 68338 block records.
  The class the host counts a block into is compared with the class the shader takes from the record for
  1.3 million vectors. Seventeen mutated copies (a rounding term, a tap, a row, a column, an edge clamp,
  the fourth sample of a row, the class or the frame of the second reference, the class of a vector with
  half a sample) are all rejected.
- **Conformance** (the gate of the feature, on every push): the conformance job decodes the whole set
  on WARP once more with `-interfast` and must get the results of `expected.txt`: 251 of 251. The smoke
  test of the timing run decodes three streams with `-gputimedetail -interfast` and with `-gputimedetail
  -interfastlists` and checks that the sections by class are there, that no block went the original way on
  a frame without scaled references, and that the dispatches and the tile threads agree on every frame.
- **Frame dumps on WARP** (`-dumpstages`, a 10-bit 1080p cut with four tiles, warped blocks and OBMC): the
  block records of a frame are the same set with and without the flag, and the frame behind the inter
  stage and behind every later stage is byte identical, with the new kernels and with the original shaders
  on the lists.
- The first run of the gate failed, and not because of the path: the two 10-bit 1080p cuts lost the D3D12
  device, as streams on WARP did now and then before. The cause was a load outside a buffer in
  `reconstruct_block`, which the larger block map of the path made fatal every time; it is fixed
  ([warp-decoder.md](warp-decoder.md), "The D3D12 device is removed").

### Timing attribution

With the flag the sections `single` and `comp_avg` of the breakdown are replaced by `single.copy`,
`single.h`, `single.v`, `single.2d` and by `comp_avg.<class of reference 0><class of reference 1>` with
the letters `c`, `h`, `v`, `2` (`comp_avg.cc` to `comp_avg.22`), each with its own time, block counts,
dispatch lines by block size and loaded bytes. `single.2d` and `comp_avg.22` are the original shaders on
the lists of that class.

### Console results

Package 0.2.45.0 (rav1g 0400bee), 4K, 300 frames, `-gputimedetail` without and with `-interfast` in one
launch:

| Clip | inter ms, base -> `-interfast` | GPU busy ms | decode fps |
|---|---|---|---|
| SVT-AV1 low-delay 40 Mbps | 7.2 -> **9.5** | 31.1 -> 34.4 | 21.4 -> 19.7 |
| SVT-AV1 random access 12.6 Mbps | 11.0 -> **12.4** | 34.8 -> 36.4 | 26.0 -> 25.1 |
| Jellyfin 40 Mbps | 19.2 -> 19.5 | 50.8 -> 51.1 | 16.5 -> 16.6 |
| Chimera 4K film grain (libaom) | 54.0 -> **48.4** | 74.6 -> 69.1 | 12.8 -> 13.7 |

md5-identical to dav1d with `-interfast` on all four, and without it on the Chimera and the 8-bit 4K
clip (the `reconstruct_block` change of the default path).

**Reading:** the fast path is correct but not the lever it was meant to be. On Jellyfin the copy class
(60 % of the fetched area, no filter at all) still takes 7.6 ms of the 19.5 (`single.copy`, overlapping
sections), h 5.2 and v 6.1 ms: skipping the filter arithmetic and the redundant rows changes the cost per
block very little. The inter stage is not bound by filtering or reference loads but by something per
block or per dispatch (thread-to-pixel mapping, small groups and occupancy, latency per dispatch, the
prediction and residual writes). Splitting each block size into classes makes the dispatches smaller and
more numerous, which is why the two SVT clips got slower; only the compound-heavy Chimera gains (10 %).
`-interfast` stays off by default. The next step for the inter stage is a profile of what a block costs
(a timestamp around a dispatch of a fixed size with copy-only blocks, the effect of more pixels per
thread and of merging block sizes into fewer dispatches) rather than more filter specialisation.

## Padding of the mode info record (2026-11-12)

What a decoder wrote depended on what the process had decoded before it. The cause was one shader
read that took two bytes of C struct padding for part of a flag.

### Symptom

- `av1-1-b10-00-quantizer-00.ivf` was wrong from frame 0 (0 of 2 frames) when it was decoded
  behind 8-bit quantizer vectors in the same process, and right alone: behind the ten vectors
  `av1-1-b8-00-quantizer-00` to `-09` with `warp_decoder` and with `rav1g-dec`, with 1 thread and
  with 4, in 7 of 7 passes (run 43245926770); behind each single one of the ten, and behind the
  first 1, 2, 4, 6, 8 and 9 of them (run 43257640361, `warp_decoder`). `-queue_size 1` and
  `-slots 3` did not change it. In the order of the conformance manifest, behind all the 8-bit
  vectors, it was right.
- `av1-1-b8-22-svc-L2T1-2.ivf` now and then had 0 of 8 frames right behind `svc-L2T1` in the same
  process and was right alone. It failed in all passes of a job or in none, more often on the slow
  runners (of 8 jobs, the 3 that failed decoded the set in 2.9 to 3.1 min, the 5 that passed in
  1.5 to 2.7), and a rerun of the job passed sooner or later. `run.py` decodes a stream that fails
  behind others again alone, so the stream never came out as wrong, only as unstable, and the
  failure was taken for one of the runner.

The stage dumps of the 10-bit vector behind an 8-bit one against the vector alone (run
43258906582): the first buffer that differs is `pred_blocks` (the block records
`gen_pred_blocks.hlsl` writes), the first stage whose pixels differ is `intra`.

### Cause

The entropy decoder writes a `MB_MODE_INFO` record (av1/common/blockd.h, 224 bytes) per block into
the mode info pool, and three shaders read the pool as `StructuredBuffer<MB_MODE_INFO>` of 32-bit
words (dx/shaders/mode_info.h). `FILTER_INTRA_MODE_INFO` is two bytes, `filter_intra_mode` and
`use_filter_intra`, and the next field is aligned to four, so the word the shader reads is

| bits | |
| --- | --- |
| 0..7 | `filter_intra_mode` |
| 8..15 | `use_filter_intra` |
| 16..31 | padding of the C struct |

`gen_pred_blocks.hlsl` took the flag as `mi.filter_intra_mode_info >> 8`: the flag and the padding.
Nothing writes the padding. The entropy decoder sets the record field by field, and the pool, a
mapped D3D12 buffer, is not cleared when the decoder is created. A first decoder of a process gets
zeroed memory. A later one can get the memory of a buffer that a destroyed decoder of the process
has used, with its content: in the failing pair the word was `0x57560059` where the vector alone has
`0x00000000`, and the bytes were data the 8-bit decoder had staged for its film grain tables.
The shader then took an ordinary intra block for a filter-intra block and listed it for the filter
intra pass, and the frame was wrong from the first intra block on.

Which memory a new buffer gets is the allocator's business, which is why the failure followed the
order and sizes of the decoders before it and, for the pair of layer vectors, the runner.

### Evidence

Run 43258906582 of a diagnostic branch (deleted since), `warp_decoder` on WARP. "Original" is the
shader as it was, "masked" the one with the fix; the padding bytes were changed in every record of
the frame right before the shader ran:

| shader | padding bytes | streams | result |
| --- | --- | --- | --- |
| original | as they are | the 10-bit vector alone | right |
| original | as they are | an 8-bit vector, then the 10-bit one | 10-bit vector: 0 of 2 frames |
| original | set to 0 | an 8-bit vector, then the 10-bit one | right |
| original | set to 0xA5 | the 10-bit vector alone | 0 of 2 frames |
| original | set to 0xA5 | `svc-L2T1-2` alone; `svc-L2T1` then `svc-L2T1-2` | 0 of 8 frames, each stream: the failure of the slow runners |
| masked | as they are | the 10-bit vector alone; behind an 8-bit one; behind the ten, `-t 1` and `-t 4`, twice each | right |
| masked | set to 0xA5 | the 10-bit vector alone; behind an 8-bit one; the same with `-interfast` | right |
| masked | set to 0xA5 | `svc-L2T1-2` alone; `svc-L2T1` then `svc-L2T1-2`, ten times | right, ten of ten |

So the padding alone makes the difference in both directions, and the failure of `svc-L2T1-2` is
the same read: its signature comes back with poisoned padding and is gone with the mask.

### Fix

`const int use_filter = (mi.filter_intra_mode_info >> 8) & 255;` (commit dda14c7 on `cpp`, merged
into `main`). `gen_pred_blocks_fast.hlsl` includes the same source. In the listings of both, one
instruction changes, `ushr r11.w, r2.w, l(8)` to `ubfe r11.w, l(8), l(8), r2.w`; the listings scan
finds no pair in them and its allow list is unchanged. The output is the same on all 251 streams
(the bits that are dropped were 0 whenever a stream was right).

### Audit: the other reads

Padding and unwritten bytes of `MB_MODE_INFO`, from the layout the layout tests of `rav1g-sys`
check against the C compiler:

| bytes | shader word | |
| --- | --- | --- |
| 93 | `wm_params.type`, bits 8..15 | padding between `wmtype` and `invalid` |
| 114..115 | `filter_intra_mode_info`, bits 16..31 | padding behind `use_filter_intra` |
| 202 | `tx_info`, bits 16..23 | `skip_mode`: a field, not written in intra frames |
| 223 | `seg_cdef_flags`, bits 24..31 | `reserved`: a field nobody writes |

Every read of the pool in the three shaders that have it, against these bytes and against what
the entropy decoder writes for the kind of block the read is made for (decodemv.c,
decodeframe.c):

| shader | read | |
| --- | --- | --- |
| `gen_pred_blocks.hlsl` (and `_fast`) | `filter_intra_mode_info >> 8` | **took the padding; masked** |
| | `filter_intra_mode_info & 255` | exact; only for a filter-intra block |
| | `wm_params.type & 0x10000` | exact: bit 0 of `invalid`, written for every `WARPED_CAUSAL` block; the padding byte is not in the mask |
| | `wm_params.mat`, `.angles` | whole 32-bit and 16-bit fields, only for a block with a valid local warp |
| | `palette_mode_info.sizes & 0xffff`, `& 0xffff0000` | exact; written for every block |
| | `block_type` (`& 255`, `<< 8 >> 24`, `>> 24`), also of neighbours | exact bytes; written for every block |
| | `modes` (`& 255`, `& 15`, `>> 8`, `>> 16 & 255`, `>> 24`) | exact bytes; `interintra_mode` and `motion_mode` only for inter blocks, which write them |
| | `intra_mode_flags` (`& 1`, `& 0x100`, `<< 8 >> 24`, `>> 24`), also of neighbours | exact bytes; `use_intrabc` is written for every block |
| | `tx_info` (`& 255`, `& 0xff00`, `>> 24 & 7`) | exact bytes; `skip_mode` is not read |
| | `interp_filters` (`& 15`, `>> 16 & 15`), `mv[0]`, `mv[1]` | whole fields, for inter and intra block copy blocks |
| | `interinter_comp.type`, `.mask_type`, `.wedge_index`, `.wedge_sign` | 32-bit fields; `type` is written for every inter block, the others for the type that has them |
| | `interintra_wedge_index`, `_sign`, `cfl_alpha_idx`, `cfl_alpha_signs`, `index_base`, `mi_row`, `mi_col` | 32-bit fields, read where they are written |
| `gen_lf_blocks_hor.hlsl`, `gen_lf_blocks_vert.hlsl` | `tx_info` (`& 0xff00`, `& 255`, `>> 24 & 7`), `block_type`, `intra_mode_flags & 0x100`, `modes & 255` | exact bytes |
| | `inter_tx_size[i] >> (8 * n) & 255` | exact bytes; only for inter and intra block copy blocks with coefficients, which write all 16 |
| | `delta_lf_from_base`, `delta_lf[4]` | 32-bit fields, read only with `delta_lf_present_flag`, which makes every block write them |

No shader reads `comp_idx`, `seg_cdef_flags`, `current_qindex` or `num_proj_ref`. One read was
wrong; nothing else was changed. `mode_info.h` now names the bytes of every packed word and marks
the padding.

The other data the CPU writes for the shaders:

- The transform block records (`tx_block_info_gpu`) and all constant buffers (`GenBlockData`,
  `GenLfData`, the inter, intra, loop filter, CDEF, restoration, film grain and transform structs)
  are made of 32-bit words only, so they have no padding. That says nothing about members the host
  does not set; see `-poisonbuf` below.
- Buffers whose unwritten parts are read are cleared by the host: the coefficient buffer when
  the decoder is created and for every frame (the entropy decoder skips zero coefficients), the
  mode info grid and the intra iteration grid for every frame.
- The block records and the edge lists (`PredictionBlock`, `InterWarpBlock`, `lf_blocks`) are
  written by shaders, not by the CPU.

That is the reading. The test of it, for the pool, is `-poisonmi` below; the layout test
`mode_info_padding_is_where_the_shaders_mask_it` of `rav1g-sys` (on `main`) fails when a change of
the struct moves or adds padding.

### Checks that stay

- **Order check** (`testing/conformance/order_check.py`, on every push, with `warp_decoder` and
  with `rav1g-dec`): the sequences above, each in one decoder process and in this order, every
  stream against its reference: `b8-00-quantizer-00` then `b10-00-quantizer-00`, and the ten 8-bit
  vectors then the 10-bit one, at `-t 1` and `-t 4`; `svc-L2T1` then `svc-L2T1-2` at `-t 4`. A
  failure is final: no stream is decoded again alone. Only a sequence that lost the D3D12 device
  runs again, whole and in the same order, at most three times.
- **`-poisonmi`** (`warp_decoder`, `rav1g-dec`; `av1_set_poison_buffers` of dx/av1_perf.h; off by
  default): the decoder fills the mode info pool with the byte 0xA5 when it creates it. A read of
  bytes the entropy decoder never wrote then changes the output of any stream at once, without the
  right decoder before it. On every push 137 streams of the set are decoded with it (ten quantizer
  vectors of each bit depth, all other vectors, one cut of Netflix Open Content) and have to give
  the results of `expected.txt`; the whole set with the flag is one of the passes of the
  workflow's `experiments` input.
- **`run.py`**: a stream that is wrong behind other streams and right alone (`unstable`) can no
  longer be listed in `expected.txt`, and the run says what it is: a defect of the decoder, not a
  reason to run the job again.

Results on WARP, commit 2bbfa69 on `cpp` (run 43265811356) and f561fbb on `main`
(run 43267606808, a slower runner):

| | `cpp`, `warp_decoder` | `main`, `warp_decoder` | `main`, `rav1g-dec` |
| --- | --- | --- | --- |
| conformance set | 251 of 251, 1.6 min | 251 of 251, 2.6 min | 251 of 251, 2.5 min |
| order check, 5 sequences, 28 streams | pass, 9 s | pass, 14 s | pass, 13 s |
| 137 streams with `-poisonmi` | 137 of 137, 24 s | 137 of 137, 36 s | 137 of 137, 36 s |

The two steps add 33 to 50 s to the `conformance` job, which sets the time of a run.

### `-poisonbuf`: the constant buffers (open)

`-poisonbuf` does the same for every buffer the CPU writes for the shaders, 17 kinds, the pool
among them. The decoder does not pass it: of the 137 streams 34 were right (run 43262574427, the
same with `warp_decoder` and `rav1g-dec`), the others were wrong from the first or the second
frame, also alone, and many runs ended with the D3D12 device removed (`0x887A0005` at the next
allocation). One buffer at a time (run 43264623408; 8 streams, each alone in a process:
`b8-00-quantizer-03`, `b10-00-quantizer-01`, `b8-01-size-196x196`, `-66x66`, `b8-05-mv`,
`b10-23-film_grain-50`, `b8-22-svc-L2T1`, the 270p cut):

| poisoned at creation | streams right |
| --- | --- |
| nothing | 8 of 8 |
| the mode info pool (`-poisonmi`) | 8 of 8 |
| each one of `idct_coefs`, `gen_mi_block_indexes`, `gen_intra_inter_grid`, `gen_block_map`, `intra_v2_buffer`, `mode_info_grid`, `idct_blocks_unordered`, `cdef_indexes`, `cdef_skips`, `loop_rest_types`, `loop_rest_wiener`, `filmgrain_rand_offset`, `output_cb_alloc`, `output_filmgrain_rand_offset`, `palette_buffer` | 8 of 8 |
| `cb_alloc`, the upload buffer of a slot's constant buffers | 0 of 8; the device removed in 3 |
| all 17 | 0 of 8; the device removed in 4 |

So the pool and 15 other buffers do not depend on what their memory held, and the constant buffers
do: some shader reads a constant that the host does not write, and gets 0 from a first decoder's
memory. Which constant is open. It matters beyond the flag: a wrong frame followed by a removed
device is what the conformance run sees now and then on WARP
([warp-decoder.md](warp-decoder.md#results)), and a decoder that gets used memory for `cb_alloc`
would be in the state the flag makes. That is a lead, not a finding. `-poisonbuf` stays a flag and
out of CI until the read is found.

### What it does not explain

- The removals of the D3D12 device on WARP ([warp-decoder.md](warp-decoder.md#results)): not this
  read. The constant buffers above are the lead for them.
- The about 75 MiB that stay behind after a decoder is destroyed in md5 runs on the console.
- Not run on the console. Whether the console hands a decoder the used memory of a destroyed one
  as WARP does is not known. The changed instruction is of a kind `gen_pred_blocks` already has
  there (`ibfe` with a literal width and offset); a console run of a package with the change is
  still to do.
