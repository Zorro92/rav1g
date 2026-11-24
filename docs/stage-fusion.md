# Doing Bale's display stage in rav1g's last pass

A design study (2026-11-23): can rav1g's last pass write the display-size, tone-mapped image that
Bale's D3D11 stage makes today from rav1g's 4K output, and what would that save? The pass is in the
decoder; branch `exp/display-abi2-opus` gives it its interface (interface version 2,
`Rav1gSettings::display`, [integration.md](integration.md#the-display-pass-interface-version-2)) in place
of the prototype's internal switch, and [bale-display-pass.md](bale-display-pass.md) lists what Bale and
its fork would change.

- [Conclusion](#conclusion)
- [What Bale's stage computes today](#what-bales-stage-computes-today)
- [Bytes and passes of a 4K60 frame today](#bytes-and-passes-of-a-4k60-frame-today)
- [Three ways to fuse](#three-ways-to-fuse)
- [What each saves](#what-each-saves)
- [Interface](#interface)
- [What Bale and the fork would change](#what-bale-and-the-fork-would-change)
- [Risks](#risks)
- [The prototype (`-displaypass`)](#the-prototype--displaypass)
- [The display pass, fast](#the-display-pass-fast)
- [The display pass, faster: reads, one pipeline a mode](#the-display-pass-faster-reads-one-pipeline-a-mode)
- [1:1, 3:2 and the dither](#11-32-and-the-dither)
- [The ABI it needs](#the-abi-it-needs)

## Conclusion

- **What the console says (Bale's per-frame logs, 60 Mbps 4K60, 2026-11-23).** HDR10 is bound by the
  GPU and the stage, SDR by the parse. HDR10 at 60 Mbps: 53 fps; rav1g waits 13.7 ms a frame (p95 26)
  for an output image, because Bale's D3D11 tone-map and downscale stage holds the images (stage
  submit to GPU done: 165 ms, against 68 ms in SDR); rav1g's own GPU time is 9.2 ms a frame, and 692 of
  1797 frames (38%) have no postfilter stage and take the full copy pass. So the stage is the lever
  for HDR, and the copy matters there too.
- **The saving is large, and it is bytes.** Today a 4K60 frame on a 1080p display moves about
  **156 MB** through the GPU between rav1g's last postfilter stage and the display-size image: rav1g
  writes the 4K P010 image (24.9 MB), Bale's area pass reads it and writes a 1080p fp32 intermediate
  (33.2 MB), and the luma and chroma passes read that intermediate again. A display pass at the end
  of rav1g's command list that reads rav1g's final frame buffer and writes the 1080p image moves about
  **31 MB**. The difference, **about 90-125 MB a frame** (the low end if the chroma pass's overlapping
  reads all hit the cache), is 1.2 to 3.1 ms of GPU at the idle state's memory rates (40 GB/s for
  copies, AGENTS.md; ~75 GB/s for the plain stores of rav1g's residual clear on the console). The tone-map arithmetic stays the same and moves from Bale's D3D11 context to
  rav1g's queue.
- **Bale can take half of it alone, now, with no rav1g change**: a compute version of its three
  passes that keeps the per-output-pixel values in group shared memory instead of the 33 MB fp32
  intermediate saves about 66-100 MB a frame (option A below). It needs no interface between the two
  projects and keeps Bale's stage for HEVC as well.
- **The rest needs an ABI change of rav1g** (the display size, the tone-map parameters and the
  meaning of the output image are new settings; [the ABI it needs](#the-abi-it-needs)). Measuring it
  needed none: **the prototype** (option B) ran in the test player and rav1g-dec behind an internal
  switch (`av1_set_display_pass`, `-displaypass 1920x1080`), so the console could say what the pass
  costs on rav1g's queue before anything is decided. Branch `exp/display-abi2-opus` has the change: interface
  version 2, `Rav1gSettings::display`, which `-displaypass` now fills. Its math is written in rav1g from the standards, in
  the order of Bale's stage, not copied (Bale is AGPL-3.0, rav1g BSD-2-Clause); where the one
  implementation should live is the owner's decision.
- **It also removes the copy pass and the image hold.** A frame without a postfilter (38% of the HDR
  clip) today takes rav1g's 4K copy (read and write, ~1.1 ms) and then Bale's stage; with the display
  pass it takes neither: the pass reads the frame buffer and writes the 1080p image. And rav1g's images
  become 1080p images that the fork copies (6 MB) into its own pool and gives back at once, instead of
  4K images held until Bale's stage is through, which is the 13.7 ms rav1g waits for an image today.
- **Not into the postfilter stage itself.** The last postfilter stage works in 64x32 (CDEF) or 64-row
  (loop restoration) groups; the area downscale with linear-light chroma needs the *final* values of a
  one-sample chroma ring and one output column around each group, which other groups compute. Taking
  them means recomputing CDEF (direction search included) for the neighbouring 8x8 blocks' edges, in
  every fusion target (CDEF, superres, loop restoration, the plain copy). A separate pass costs one
  dependent step (~18 us) and a 25 MB read and has none of that (option C below).

## What Bale's stage computes today

Bale's stage is `bale_gpu.dll`'s tone map (ABI 1, `native/bale-gpu/include/bale_tonemap.h`), called by
the FFmpegInteropX fork for every rav1g frame (`GpuDecoderSampleProvider::OutputFrame`): the fork's
D3D11 context waits on rav1g's shared output fence, `BaleToneMap::Process` reads rav1g's P010 image
(a D3D11 texture with `SHADER_RESOURCE | UNORDERED_ACCESS`) and writes a texture of the fork's output
pool (`RENDER_TARGET | SHADER_RESOURCE`), which goes to the player. Without a stage the fork copies
the 4K image into a texture of its pool (`CopyResource`), so that the player never holds a decoder
image.

The stage runs for 4K AV1 on a smaller display in every case: HDR10 (PQ), HLG and BT.2020 SDR are
tone mapped or gamut-converted to BT.709 SDR, and BT.709 SDR is processed only to downscale it
(`BALE_TONEMAP_SDR709`, "the Xbox player's downscale of 4K SDR leaks driver state per open").

The math (`native/bale-gpu/shaders/tonemap_common.hlsli`, `tonemap_math.hlsli`; CPU reference
`src/tonemap/cpu.rs`; all fp32):

1. **Per source pixel** (`sample_linear`): the chroma at the luma pixel's position by bilinear
   interpolation of the four nearest chroma samples, sited by the stream's `chroma_location`
   (`chroma_offset`: left, center, top-left, top, bottom-left, bottom); Y'CbCr to R'G'B' by the
   stream's matrix (BT.2020 or BT.709 weights) and range; the transfer decoded sign-preserving
   (PQ EOTF, HLG inverse OETF + OOTF at 1000 nits, or BT.1886 power 2.4 for SDR); for PQ and HLG
   BT.2020 to BT.709 and the tone curve on the largest channel (BT.2390 EETF with source peak 1000
   nits and SDR white 203 nits, or Hable), optional desaturation; BT.2020 SDR is gamut-converted only,
   BT.709 SDR scaled only. The result is display-linear BT.709 RGB, unclipped.
2. **Per output pixel** (`area_linear`): the average of step 1 over the source pixels its footprint
   covers, each weighted by the overlap of its unit square with the footprint (exact area weights;
   at 2:1 exactly four pixels with weight 1/4). Then one clip to 0..1 (`finish_linear`), BT.1886
   encode (power 1/2.4) and BT.709 Y'CbCr (`encode_linear`).
3. **Output luma**: the encoded Y' of step 2, quantized to 10-bit limited range (`64 + 876 Y'`,
   rounded) for P010, or to 8 bits with an 8x8 Bayer ordered dither (`BALE_TONEMAP_OUTPUT_NV12_DITHERED`)
   for NV12 on an 8-bit display (not used for AV1: AV1 output is always P010).
4. **Output chroma** (left-sited): the linear RGB of step 2 mixed over the footprint 1/4, 1/2, 1/4
   across output columns 2i-1..2i+1 and 1/2, 1/2 over rows 2j, 2j+1, then clipped, encoded and
   quantized once.

Downscaling to 1920x1080 from 3840x2160 runs three draws: `tonemap_area_ps` writes step 2's unclipped
average into a 1920x1080 `R32G32B32A32_FLOAT` intermediate (31.6 MiB), the luma and the chroma pass
read it (`BALE_STAGED`). At 1:1 there are two draws and no intermediate. The size: the source fitted
into the display, aspect kept, even, at most 4:1 per axis (`VideoFrameProcessor::SelectOutputSize`).

Fixed per stream (`BaleToneMapConfig`): output size, curve, source peak 1000 nits, SDR white 100
nits (the 203-nit EETF endpoint), desaturation 0, visible source size. Per frame
(`BaleToneMapFrameInfo`): transfer, primaries, matrix, range, chroma location, mastering display
and content light metadata (passed, not used by the curve today: the peak is the configured 1000).
Accuracy: the GPU is held to the CPU reference within 3 NV12 codes / 12 P010 codes at creation; the
staged and per-plane paths agree to at most one code.

## Bytes and passes of a 4K60 frame today

3840x2160 10-bit to 1920x1080 P010, the frame written by CDEF (the common case at 40 Mbps:
1176 of 1189 frames of the 40 Mbps clip in the 0.6.0-alpha.2 timing run):

| Pass | Reads | Writes | Queue |
| --- | ---: | ---: | --- |
| CDEF, fused output: the 4K P010 image | (its own input, unchanged) | 24.9 MB | rav1g, D3D12 |
| fence wait, D3D11 side | | | |
| area pass: 4K luma + chroma (bilinear, mostly cache hits) | 24.9 MB | 33.2 MB (fp32 RGBA 1080p) | Bale, D3D11 |
| luma pass | 33.2 MB | 4.1 MB | Bale |
| chroma pass (six taps, mostly cache hits) | ~33.2 MB | 2.1 MB | Bale |
| **total of the output path** | **~91 MB** | **~64 MB** | **~156 MB, 3 draws, 1 cross-API wait** |

A frame without CDEF, superres and loop restoration (all frames of the low-delay clip, 248 of 590 of
the 100 Mbps clip) adds rav1g's copy pass: 24.9 MB more read (the copy reads the frame buffer, CDEF
does not need to). Bale's own estimate of the stage after its 2026-11-22 restructuring is 4-4.5 ms a
frame ("ALU share scaled by 0.4, plus well under 1 ms of traffic"), not yet measured on the console;
at the idle state's 40 GB/s the traffic alone is ~3.9 ms, so the stage is likely bound by memory there.

## Three ways to fuse

**A. Bale alone: one compute pass, no intermediate.** The three draws become one compute dispatch:
a group takes a tile of output pixels plus a one-pixel ring (the chroma footprint reaches one output
column left and right; rows pair up inside an even-aligned tile), computes step 2 for each into
group shared memory, and writes luma and chroma from there. At 2:1 a 32x16 output tile with its ring
is 34x16 values: 6% more tone-map evaluations, no 33 MB write and no 33-66 MB of reads. Exactness:
the same functions in the same order; the area sum and the chroma mix read the same fp32 values.
Nothing changes between the projects. This is Bale's change and needs D3D11 compute (cs_5_0 or SM6
through D3D12; the Xbox has both).

**B. rav1g: a display pass at the end of the frame's command list.** After the last stage that
writes final samples (CDEF, superres, loop restoration, or the reconstruction for a frame with none of
them) and film grain, one compute pass reads the frame buffer and writes the display-size P010 image
into the application's texture, with the math of option A. The 4K output image is not written at all.
The frame buffer is there anyway for a frame that is referenced; for an unreferenced frame
(`refresh_frame_flags` 0, AV1 7.20), whose frame buffer writes are skipped today, the last stage
writes the frame buffer instead of the 4K image (the same 24.9 MB). Film grain (AV1 7.18.3) is applied
to the frame before the downscale: the grain pass writes a 4K frame into the decoder's back buffer
(24.9 MB written and read again), or, later, grain is computed per sample inside the display pass
(the grain block's template offset and the overlap blend are known per sample). One dependent step
(~18 us). The copy pass of frames without a postfilter disappears.

**C. rav1g: inside the last postfilter stage.** As B, but each CDEF group (64x32 luma, 32x16 chroma)
writes its own output pixels. At 2:1 its 32x16 output pixels cover its luma exactly, but the bilinear
chroma of its edge pixels needs final chroma one sample outside the group, and the output chroma
footprint needs output pixels one column outside: values that neighbouring groups compute. A group
would have to filter a ring of its neighbours' samples too: the direction search (the costliest part
of CDEF) of up to 28 neighbouring 8x8 blocks for the group's own 32, and a 2-sample halo more, in
each of four fusion targets (CDEF, superres, two loop restoration shaders)
plus the copy path. At ratios other than 2:1 footprints straddle the groups too. It saves only the
25 MB read of option B. Not worth it now; revisit if B is measured and the read shows up.

## What each saves

Per 4K60 frame on a 1080p display (bytes as in the table above; ms at 40 GB/s, the idle state's copy
rate, and at 75 GB/s, the store rate of rav1g's residual clear on the console in the idle state):

| | Bytes moved for the output | Saving against today | ms saved | Passes and waits | Memory |
| --- | ---: | ---: | ---: | --- | --- |
| Today | ~156 MB | | | 3 draws + 1 cross-API wait | 4 x 23.7 MiB 4K images, 31.6 MiB intermediate |
| A (Bale only) | ~57-90 MB | ~66-100 MB | 0.9-2.5 | 1 dispatch + 1 wait | -31.6 MiB |
| B (rav1g display pass) | ~31 MB | ~90-125 MB (~115-150 MB on frames that copy today) | 1.2-3.7 | 1 dispatch, no D3D11 pass | -71 MiB of images (4 x 5.9 MiB at 1080p), -31.6 MiB intermediate |
| C (in the last stage) | ~6 MB + halo work | ~150 MB | as B, plus the 25 MB read, minus the halo work | 0 extra | as B |

The arithmetic (tone map, about 8.3 M source-pixel evaluations at 2:1, each a handful of `pow`) is
the same in A, B and C; in B and C it moves onto rav1g's queue. rav1g's GPU time per frame then grows
by it (Bale's estimate: ~3 ms of the 4-4.5 ms) while the app's total falls by the bytes: 8.9 + ~3 ms is
still inside rav1g's 14 ms at 60 fps, and Bale's 15% share frees up. At 1:1 (an HDR stream on a 4K SDR
display) B saves less: Bale's two draws read the 4K image twice and write the 4K output, B reads the
frame buffer once and writes the output: ~25-50 MB.

## Interface

In short (the exact form is under [The ABI it needs](#the-abi-it-needs)): the application gives the
display size and the stage's parameters once, in the decoder's settings; rav1g asks for display-size
P010 (or NV12) images and writes them; the frame it hands back says that it was processed and what the
source was. Per frame nothing comes from the application: rav1g has the frame's colour description
and HDR10 metadata itself. A change of the target (display mode change) recreates the decoder, as a new
sequence does. Internal A/B switches stay out of the ABI (`av1_*` in `av1_perf.h`).

## What Bale and the fork would change

- **Fork** (`GpuDecoderSampleProvider`, `BaleRav1g.h`, `GpuDecoderImagePool.h`): when the stage would
  run and rav1g has the display pass, give the display size and the stage's parameters in the decoder
  settings instead of creating `BaleToneMap`; size the rav1g image pool to the display (P010, UAV); in
  `OutputFrame` replace `frameProcessor->Process` by a `CopyResource` of the 1080p image into a
  texture of its own pool (6.2 MB, kept because the player may hold samples and must never hold
  decoder images: the deadlock the fork's comment describes), or, as a second step, let rav1g write
  the player's textures directly when the pool sizes allow it. Keep `ToneMappedOutputMark`, the media
  type at the display size, and the fallback to `BaleToneMap` for decoders without the pass.
- **Bale**: pass the same server parameters it gives the stage (curve, peak, white, desaturation,
  output depth), keep the stage for HEVC, H.264 and VP9; the stats pane's "Stage GPU" becomes rav1g's
  display pass for AV1 (a GPU mark of its own in `av1_perf_frame`).
- **One math in two places**: either the tone-map math moves into rav1g under rav1g's licence (the
  owner is the author of both; Bale's `tonemap_math.hlsli` is "independently expressed" and has a Rust
  mirror in `cpu.rs`), or rav1g takes the same HLSL file from Bale as a build input. Either way CI of
  both projects holds the GPU result against the same CPU reference.

## Risks

- **Exactness against Bale's own stage.** Not bit-exact by construction: the same fp32 functions in a
  compute shader under DXC, against pixel shaders under FXC/DXC. Bale already accepts one code between
  its own paths and holds the GPU to the CPU reference within 3 NV12 / 12 P010 codes; the display pass
  would be held to the same `cpu.rs` reference with the same tolerances, on WARP in CI and by a
  console check. The decoded frame is not affected: rav1g's per-frame md5 (taken from the frame
  buffer, not the display image) and the Argon results stay the gates, and the frame buffers that
  later frames read are untouched.
- **Colour description.** The stream's sequence header may say "unspecified" where the container says
  more; the fork merges the two today. Without the `assumed_*` settings rav1g would tone map such a
  stream differently from Bale's stage.
- **HDR metadata.** Bale's curve uses the configured 1000-nit peak, not the stream's mastering
  metadata; the same parameters have to reach rav1g, and a later per-frame peak (from MaxCLL or the
  mastering display) has to be decided in one place.
- **Odd and changing sizes.** Bale declines odd source sizes; AV1 frames may change size within a
  sequence (`frame_size_override_flag`, render size) and superres changes the coded width: the display
  pass takes each frame's upscaled size as its source and keeps the output size of the decoder.
- **Film grain** has to be applied before the downscale (AV1 7.18.3): an extra 4K write and read per
  grain frame in the simple form.
- **GPU time attribution.** rav1g's GPU per frame grows by the tone-map arithmetic; its 85% envelope
  (14 ms at 60 fps) still holds at today's 8.9 ms + ~3 ms, but the stats must show the display pass
  apart from decoding.
- **Debug paths.** Bale's `gammachroma` and `nostaged` switches and its stage timing would need
  equivalents in rav1g (an `av1_*` switch and a GPU mark).

## The prototype (`-displaypass`)

Option B, first behind an internal switch (`av1_set_display_pass`, removed on `exp/display-abi2-opus`):
now `Rav1gSettings::display` (interface version 2), which `-displaypass WxH` of the test player and of
rav1g-dec fills. A decoder with it asks for images of WxH for every frame, P010 or NV12 by its depth
setting. How the pass was made fast after the first console measurement: [The display pass,
fast](#the-display-pass-fast) and [faster](#the-display-pass-faster-reads-one-pipeline-a-mode). What
this section says of the prototype's limits is partly superseded there: monochrome frames take the pass,
the colour tags assumed for "unspecified" are settings, and `Rav1gFrame::color` describes the image.

- **The pass** (`libav1/dx/shaders/display_pass.hlsl`, called from `copy_semi_planar` in `output.rs`):
  one dispatch at the end of the frame's command list in place of the output copy, a 256-thread group
  per 64x64 output pixels ([below](#the-display-pass-fast)). Every operation of [What Bale's stage computes](#what-bales-stage-computes-today)
  in the same fp32 order: area weights, bilinear chroma by siting, sign-keeping decode, BT.2390 EETF
  (source peak 1000 nits, white 100), HLG OOTF, BT.2020 to BT.709, one late clip, inverse BT.1886, the
  left-sited linear chroma mix, limited-range quantization (10-bit for P010, 8-bit for NV12, rounded,
  no dither). The source's transfer, matrix,
  range and chroma position are those of the sequence header (PQ: tone map; HLG: OOTF and tone map;
  BT.2020 SDR: gamut; else BT.709 scale only).
- **What changes around it**: the last postfilter stage does not write the output image (the fusion is
  off for such frames, `out=copy-switch` in the timing run) and writes the frame buffer of every frame,
  unreferenced ones too, since the pass reads it. The decoded frames and the reference frames are the
  same bytes; only the output image differs.
- **Film grain** (AV1 7.18.3) is applied before the downscale: the film grain pass writes the grained
  planes into the back buffer in the frame buffer's layout, and the display pass reads them there (25 MB
  more written and read at 4K 10-bit, grain frames only).
- **Not in the prototype**: monochrome frames (they keep the normal output), the NV12 dither of Bale's
  stage (8-bit output is rounded), film grain with `-tryhdr10x3` (not applied, logged once), the staged
  path (a texture shaders may not write:
  logged, no frame written), the container's colour tags, the Hable curve and desaturation, Bale's
  `gammachroma` math. `Rav1gFrame::color` still describes the stream.
- **Cost on rav1g's queue**: one dependent step (~18 us), a read of the frame buffer (24.9 MB at 4K
  10-bit) and a 6.2 MB write at 1080p, and the arithmetic of 8.3 M source-pixel evaluations, against
  the fused output stores (frames with CDEF) or the 49.8 MB copy (frames without a postfilter) of
  today. Registers (`shader_regs`, gfx1030 wave64): 60 VGPRs, 15 KB of group memory, 8 waves a SIMD
  (the first prototype: 58, 4.6 KB, 7).
- **Checked**: CI job `rust-output` (WARP) decodes the PQ stream `syn-hdr10-10b-1080p`, the SDR
  stream `syn-tiles-10b-1080p` and the film grain streams `av1-1-b10-23-film_grain-50` and
  `av1-1-b8-23-film_grain-50` of the conformance set once as they are and then with `-output p010
  -displaypass` at half their size (the quad path) and at two thirds (the area path), and
  `testing/display_pass/check.py` holds every code of the display images against a float32 model of the
  pass (`testing/display_pass/model.py`, numpy) computed from the first run's frames (with their grain),
  at most one code apart (fp32 on two compilers); a code further off is printed with its place and its
  source pixels. `testing/display_pass/run.sh` (CI job `sync-check`, and precheck) runs the shader
  compiled as C++ on 17 test cases under the adversarial thread schedule of `testing/common/group_sched.h`
  and holds them against the same model, with eight mutations that must be rejected. The model follows the same description; holding both against Bale's own
  CPU reference (`cpu.rs`) is a step for Bale's side.
- **On the console**: the test player's timing run with `-output p010 -displaypass 1920x1080` (or
  `-interop -displaypass 1920x1080`) against the same task without it, on the 40 and 60 Mbps clips and
  the HDR10 one: `copy_plane` in the stage table is the display pass on every frame, `gpu busy` the
  sum. Bale's "Stage GPU" for the same clips is the other side of the sum.

## The display pass, fast

The prototype as first measured (console, idle state, 2026-11-23, `-output p010 -displaypass
1920x1080`): 3.48 ms a 4K frame in the `copy_plane` slot (5.49 on the frames of a 4K50 stream), against
1.10 ms for the plain copy it replaces. It reads 24.9 MB and writes 6.2 MB, ~0.8 ms at the idle state's
~40 GB/s, so it was bound by arithmetic (AGENTS.md: 1.86 TFLOP/s for the app, compute not slowed in the
idle state): per source pixel the PQ EOTF of three channels and the EETF of the largest, seven `pow`
pairs with their divisions, about 35 transcendental operations (quarter rate on RDNA2), for 8.3 M source
pixels; and its 64-thread groups computed 264 values with 64 lanes (a fifth pass for 8 lanes).

What the pass does now:

- **Tables for PQ.** The EOTF of a signal, and the tone-map gain `eetf(peak) / max(peak, 1e-6)`, which
  is a function of the largest R'G'B' signal alone (the EOTF is monotonic), are tables the host makes
  once a process (`crates/rav1g/src/host/display_pass.rs`) and copies into the frame's constant memory
  (5.5 KB a frame, bound as `t0`); every group reads them into group memory (510 groups at 1080p: 3 MB
  of cache reads). An interval holds a cubic (Hermite, from the function's float64 values and one-sided
  derivatives at its ends): one 16-byte read and three FMAs a value, within 1e-6 of the function
  wherever the light is above 1e-5 of SDR white. The EOTF has 64 intervals up to a signal of 1/32 and
  192 above; the gain 64 from the EETF's knee (below it the gain is 1 / the 203-nit target) to the
  source peak's signal, where the curve meets its clip, and 32 above. That accuracy is needed, not only
  nice: a first version with linear interpolation (1024 + 512 entries, 1e-4 relative, within 0.08 of a
  code for any grey) differed from the model by 2 codes on WARP, on out-of-gamut pixels whose BT.709
  channels cancel to almost nothing before the 1/2.4 power, which turns 1e-5 of light into a few codes.
  Below a largest signal of 4e-4 (~1e-6 nits) the gain is proportional to the peak (the 1e-6), and the
  peak's nits are computed there: a branch near-black out-of-gamut pixels take. HLG takes the gain table
  on log2 nits (one line, 1000 nits on a node); SDR's power 2.4 stays a `pow` (no table is cheaper at the
  same accuracy) and loads no table.
- **BT.2020 to BT.709 once per output pixel.** The conversion is linear and so is the area average; the
  gain is folded into the sum, the Y'CbCr constants come as inline constants (scalar registers).
- **The 2:1 quad path** (exactly 2:1 with 4-byte aligned planes: 3840x2160 to 1920x1080): a thread takes
  2x2 output pixels, reads each of its four source rows with one 8-byte load (4 bytes at 8 bits; a wave
  reads 256 contiguous bytes a row) and U and V of the row's two chroma rows at three columns; the sited
  chroma positions have constant weights (source columns at chroma columns c, c + 1/2, c + 1, c + 3/2),
  exact in float32 like the model's. Other ratios take the model's area loop with the same tables.
- **Groups of 256 threads, 64x64 output pixels.** Four quads a thread; each output chroma sample's left
  neighbour column comes from the thread on the left through group memory (two buffers, one group sync a
  quad row), the column left of the tile from the first 64 threads (one pixel each, 1.6% more pixels).

**Expected on the console** (not measured here; the parent runs the A/B): the PQ quad path is about 62
VALU operations a source pixel (RGA's listing: 47 in the PQ block with the near-black branch, which is
skipped, plus the decode and the sum), about 250 an output pixel, and the encode (three `pow` for luma, a
quarter of three for chroma: 7.5 transcendental operations), the BT.709 conversion, the chroma mix and
the stores about 45-90 cycles more: ~340 lane cycles an output pixel, 0.7 G for 2.07 M output pixels:
**0.76 ms with the slice's ALUs fully busy** (1.86 TFLOP/s is 0.93 T lane operations a second). With 8
waves a SIMD the 31 MB of traffic (0.7-0.8 ms at 40-45 GB/s, the copy pass's rate) overlaps it: about
**0.9-1.1 ms a 4K frame** for 1080p, against 3.48 ms, and below the 1.10 ms of the copy it replaces. The
4K50 stream's frames should take the same per frame (the work is per output pixel and no longer
depends on the content: the old EETF branch was divergent on bright pixels). The pass runs once per
output image: in the frame's command list for a shown frame, in an output context's for a
`show_existing_frame`, never for a hidden frame (`copy_output` only for `image_state != 0`); the
show-existing copies have their own line in the timing run (`GPUTIME show-existing copies`), so their
pass is not in the per-frame `copy_plane` mean.

## The display pass, faster: reads, one pipeline a mode

Console, idle state, SDR 10-bit 4K60, `-output p010 -displaypass 1920x1080` (the parent's run of
5c12ba1): 1.98 ms a 4K frame for the pass (3.48 before the tables, 1.10 for the plain copy it replaces);
rav1g's GPU busy 8.88 to 10.67 ms a frame at 40 Mbps, 9.09 to 10.65 at 60 Mbps; show-existing copies
1.12 to 2.19 ms. The estimate of 0.9-1.1 ms above counted arithmetic; SDR has less of it than PQ (a
`pow` pair a channel, no tables), so the time went elsewhere.

**Why: the reads waited one by one.** The RDNA2 listing of 5c12ba1 (`testing/shader_regs`' tools: DXC
to SPIR-V, LLPC for gfx1030 in wave64, the nearest public compiler to the console's) has every read of
the quad path as `buffer_load_dword` followed at once by `s_waitcnt vmcnt(0)`, each in a basic block of
its own: `sample_at` and `pair_at` branched on the frame's sample size (`eight_bit()`) around every
load, and a quad read 36 values (four rows of luma, and for each row two chroma rows of U and V at
three columns, the same rows again and again). So a quad waited for 36 memory round trips in a row,
which 8 waves a SIMD do not hide: with ~600-1000 cycles a round trip on a shared GPU that is far more
than the quad's ~2400 cycles of arithmetic a wave.

**What changed** (`display_pass.hlsl`):

- **No branch around a read.** The sample size is a shift and a mask (`load_run`, `run_sample`): an 8-byte
  read at the dword below the sample and a field of it, the same code at 8 and 16 bits.
- **A quad reads 16 times 8 bytes, a step 8, issued together.** A quad is two steps of two source rows.
  A step reads its two luma rows (8 bytes: four samples at 16 bits) and three chroma rows of U and of V
  (8 bytes each: the three columns c, c + 1, c + 2, the third reaching at most two samples into the 16
  samples of right border every frame buffer row has), then computes. The chroma rows of a step are
  those its two rows interpolate between, from o.y - 1 + p (between rows) or o.y + p (colocated): the
  same values as before, exact in float32. LLPC issues a step's reads in one group (two waits a step).
- **The edge column** (the output column left of the 64x64 tile) is 32 quads of the first 32 threads
  through the same code (their right column), not 64 area evaluations through the general loop; one call
  site for both (a loop from step -1).
- **One pipeline a transfer mode** (`display_pass.hlsl` PQ, `display_pass_sdr.hlsl`,
  `display_pass_hlg.hlsl`: a define and an include): a shader's registers are those of its most
  demanding path, and with the mode a constant the quad's code has no branch either. RDNA2/LLPC, VGPRs
  and waves a SIMD: SDR 63 (8 waves, LDS 9 KB: no tables), HLG 65 (6), PQ 112 (4; the 2:1 path alone 96,
  the PQ table reads of a step are hoisted together). All three in one shader: 114 (4 waves for every
  mode). The C++ check compiles the file with `DISPLAY_MODE_FROM_FLAGS`, every mode in one.
- **The output depth is its own flag** (bit 7): NV12 or P010 whatever the frame's depth
  (`Rav1gDisplay::depth`); the gamut flag (bit 2) follows the primaries, not the matrix (a PQ stream with
  BT.709 primaries takes no gamut step); BT.601 matrices have their weights.

**Expected on the console** (not measured here; the parent runs the A/B). From the RDNA2 listing of
`display_pass_sdr`: a 16-bit step is 311 VALU instructions (48 of them `v_log`/`v_exp`), so a quad 622,
and the encode of its four luma values and its chroma sample, the stores and the loop about 200 more:
~820 a quad, 111 transcendental. At one cycle a lane for an instruction and four for a transcendental,
~1150 lane cycles a quad; 518400 quads at 1920x1080 and 3% for the edge column: 0.62 G lane cycles, **0.66
ms** with the slice's ALUs busy (1.86 TFLOP/s is 0.93 T lane operations a second). The traffic, 24.9 MB
read and 6.2 MB written, takes **~0.69 ms** at the plain copy's measured rate (49.8 MB in 1.10 ms, 45
GB/s). With 8 waves a SIMD and two waits a step the two overlap: **~0.7-0.9 ms a 4K SDR frame** (from
1.98), so rav1g's GPU busy ~9.6 ms a frame at 40 Mbps (from 10.67; 8.88 without the pass), and the
show-existing copies ~1.0 ms (from 2.19; the pass runs for each). PQ: a step 669 VALU (40
transcendental, 32 table reads in group memory), ~1.05 ms of arithmetic at 4 waves a SIMD: **~1.1-1.3
ms**; HLG a step 621 (48), ~1.1-1.3 ms at 6 waves. The console's compiler is not LLPC: the numbers are the basis,
the console's A/B is the measurement.

**Checked**: `testing/display_pass/run.sh --mutate`: 23 cases (17 before, and an 8-bit frame into P010, a
10-bit one into NV12, BT.601, PQ with BT.709 primaries, colocated SDR, colocated 8-bit HLG) within one
code of the model (most within zero), and 12 mutations rejected (among them the chroma rows of the second
step, the right chroma edge, the edge column, the 8-bit chroma pair and the output depth).

## 1:1, 3:2 and the dither

Branch `exp/display-v2-opus` (2026-11-23). Bale's stage runs for an HDR source on an SDR display at any
size, 1:1 included (a 4K SDR TV showing HDR10), and for an SDR source larger than the display; the display
is the console's HDMI mode, 1920x1080 or 3840x2160 in practice, 2560x1440 possible, with the source fitted
into it. So 1:1 and 2:1 are the sizes that matter, 3:2 the next. Before this branch only 2:1 had a fast
path; 1:1 and 3:2 took the area loop: per output pixel a loop over its source pixels, each with nine
separate 4-byte reads (luma and the four chroma neighbours of U and V) the compiler waits for one by one,
and each source pixel decoded again by every output pixel that overlaps it.

**The block paths** (`quad_block`, `display_pass.hlsl`). At exactly n:2 in both axes a thread's 2x2 output
pixels come from an n x n block of source pixels: n 2 at 1:1, n 3 at 3:2. A quad reads, all together, n
luma rows of 8 bytes and a window of three chroma rows of three columns of U and V (8 bytes each); each
source pixel is decoded once, its chroma interpolated at its siting from the window (the weights 0, 1/4,
1/2, 3/4: exact in float32 like the model's), and weighted by its exact overlap with the output pixels:
1 at 1:1; at 3:2, along an axis, 1, 1/2, 0 for the first output pixel and 0, 1/2, 1 for the second, the
sums over 2.25 (the area weights of 3840 to 2560). The output chroma, the edge column of a tile and the
stores are the 2:1 path's. The sizes: 3840x2160 to 3840x2160 and 1920x1080 to 1920x1080 (1:1), 3840x2160
to 2560x1440 and 1920x1080 to 1280x720 (3:2); a ratio that is not exact in both axes (3840x1600 fitted
into 2560x1440 is 2560x1066) keeps the area loop.

**Cheaper encode.** After the late clip (every transform but BT.709 SDR's scale) the values are 0..1, so
the inverse BT.1886 is the power alone, without the sign and the cap of the unclipped path: 4 instructions
a channel less, 60 a quad, the same values.

**Registers** (RDNA2, LLPC, `testing/shader_regs`; the pipelines hold every path): PQ 114 VGPRs (from
112), SDR 64 (63), HLG 68 (65): the same 4, 8 and 6 waves a SIMD. The paths alone (a variant of the shader
with one path, not built): PQ 2:1 67, 1:1 62, 3:2 64; SDR 68, 35, 56.

**Expected on the console.** Counted from the RDNA2 listing of variants with one path each (the 2:1
step loop unrolled and the rare near-black branch of PQ taken out, so that the count is what a quad
executes): VALU instructions a quad, of them transcendental (quarter rate, four cycles), and lane cycles
(one an instruction, four a transcendental). Quads: the output pixels / 4, plus 6% for the edge column
(one of a group's four waves computes it, half full). The slice's ALUs do 0.93 T lane cycles a second.
2:1 measured 1.01 ms (SDR) and 1.19 ms (PQ) on the console where this count gives 0.78 and 0.88 ms: the
pass runs at ~1.3 times its ALU time (SDR 1.30, PQ 1.35), and that factor scales the others.

| Path, mode | VALU a quad (transc.) | Lane cycles a quad | Lane cycles an output pixel | Quads | ALU time | Expected | Traffic (read + written) |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| 2:1 SDR (1080p) | 933 (126) | 1311 | 328 | 0.55 M | 0.78 ms | 1.01 ms measured | 31 MB, 0.69 ms |
| 2:1 PQ | 1368 (30) | 1458 | 365 | 0.55 M | 0.86 ms | 1.16 ms (1.19 measured before the encode change) | 31 MB |
| 2:1 HLG | 1512 (126) | 1890 | 473 | 0.55 M | 1.12 ms | ~1.45 ms | 31 MB |
| 3:2 SDR (1440p) | ~913 (84) | ~1165 | 291 | 0.98 M | 1.22 ms | ~1.6 ms | 36 MB, 0.80 ms |
| 3:2 PQ | 1154 (30) | 1244 | 311 | 0.98 M | 1.31 ms | ~1.8 ms | 36 MB |
| 3:2 HLG | 1238 (84) | 1490 | 373 | 0.98 M | 1.57 ms | ~2.0 ms | 36 MB |
| 1:1 SDR (4K) | ~578 (54) | ~740 | 185 | 2.20 M | 1.75 ms | ~2.3 ms | 50 MB, 1.10 ms |
| 1:1 PQ | 677 (30) | 767 | 192 | 2.20 M | 1.81 ms | ~2.45 ms | 50 MB |
| 1:1 HLG | 713 (54) | 875 | 219 | 2.20 M | 2.07 ms | ~2.7 ms | 50 MB |

(SDR: BT.709 scale only; its pipeline also holds the clipped BT.2020 encode, which the static count
includes and the dynamic one, here, does not. The PQ near-black branch: pixels whose largest signal is
below 4e-4, rare.) A source pixel costs about the same on every path (PQ ~100 instructions: three EOTF
table reads, the gain table, the decode and the chroma interpolation); what grows towards 1:1 is the
encode, which is per output pixel (three powers for luma, a quarter of three for chroma: ~80 of PQ 1:1's
192 lane cycles a pixel). At 1:1 the traffic is the copy pass's (1.10 ms) and stays under the arithmetic.

Against what the stage costs today: at 1:1 Bale's stage evaluates each source pixel once for luma and
six times a chroma sample (its 1:1 path has no intermediate), 20.7 M evaluations a 4K frame, which at 2:1
cost ~10 ms on the console before Bale's restructuring; the fork also keeps copying the image into its
pool (a 4K P010 `CopyResource`, ~1.2 ms at the idle state's copy rate), which a later step can remove
([bale-display-pass.md](bale-display-pass.md)).

**The dither** (`threshold`, [integration.md](integration.md#the-display-pass-interface-version-2)):
about 6 instructions a sample (two multiplications, an addition, a shift, a conversion and an FMA), 36 a
quad, only with the flag: +0.03 ms at 1080p, +0.1 ms at 4K (NV12; P010 with `RAV1G_DISPLAY_DITHER_ORDERED`
the same).

**Checked**: `testing/display_pass/run.sh --mutate`: 48 cases (26 before; the new ones 1:1 and 3:2 in
every mode, both sitings, 8-bit frames, full range, near-black out of gamut, every code at random, and the
dither on each path and depth), all within one code of the model and at most 0.5% of a plane's codes one
apart; 24 mutations rejected, 12 of them new (the block paths' chroma rows, columns, window edge, first
sample, weights and third row; the odd chroma edge; the dither threshold and its transposed Cr). CI
`rust-output` runs the decoder on WARP at 1:1 too, at 0.6 of the size (the area loop), NV12 dithered and
rounded, and P010 dithered.

**Also fixed**: the area loop and the model took the last chroma column of an odd width as `w / 2 - 1`;
an odd frame has `(w + 1) / 2` chroma columns (`(w + ss_x) >> ss_x`), so its last luma column took its
chroma from one column too far left. Only the area loop sees odd sizes.

## Catmull-Rom chroma, the source peak from the metadata (interface version 3)

Branch `exp/display-parity-opus` (2026-11-24), following Bale's stage of the same day so that the two look
the same (the rule and the filter: [integration.md](integration.md#the-source-peak-and-the-input-chroma-interface-version-3)).

**The source peak** costs no GPU work: it is the tables' parameter (`display_pass.rs` `source_peak`, per
frame from the metadata `cm.hdr_metadata` carries), and the tables are made again on the CPU only when it
changes (once a stream in practice, about 2000 float64 evaluations). It exposed a fault of the tables of
version 2: the shader positions the PQ gain table by the smaller of two lines (the knee to the peak's
signal, the peak to 1), which is right only while the knee's 64 intervals are at least as dense as the 32
above the peak; from a peak of about 1200 nits on (4000-nit grades: all the time) they are not, and
the pass read the wrong intervals (codes up to 700 apart in the C++ check at a configured peak of 10000).
The shader now takes the larger of the two lines when the upper one is the steeper (a uniform choice from
the two slopes it has as constants); the table keeps its nodes at the knee, the peak and 1, and the Rust
test holds it to 2e-6 from 150 to 10000 nits and whites of 80 to 400 (an even spacing from the knee to 1
instead would put the peak's change of curvature inside an interval: 1.5e-5).

**The chroma.** A source pixel's chroma takes a 4x4 Catmull-Rom filter of the coded samples (the bilinear
version: 2x2). How the reads are organised (`display_pass.hlsl`, the comment of `sample_linear` and of the
paths):

- *Per source row, not per window.* At 2:1, 1:1 and 3:2 a thread takes one source row of its quad a step:
  the row's luma (8 bytes) and its four tap rows of U and V, five chroma columns from the column left of
  the quad's first (12 bytes at 16 bits from the dword boundary, 8 at 8 bits), 9 reads issued together.
  The five columns are filtered down with the weights of the row's parity and the siting (constants: the
  phases are 1/4 and 3/4 between rows, 0 and 1/2 colocated), then across at the row's four (or two, three)
  source columns (phase 0 or 1/2), then clamped. Neighbouring steps and threads read overlapping rows: 36
  reads a 2:1 quad (16 bilinear), from the cache (the chroma of a 64x64-pixel group is 16 KB at 2:1 and
  10-bit, read by its 256 threads together). A window of five rows shared by two steps, or the block's whole window,
  reads less but holds it in registers: 2:1 PQ 128 VGPRs, 3:2 PQ 167 (2 waves a SIMD), against 91 and 107
  a row a step.
- *No group memory.* A group-shared chroma tile would read each sample once, but the tile of a 2:1 group
  (68 rows of 69 samples, U and V) is 18.8 KB on top of the 12.5 KB the pass has: 31 KB a group, two
  groups a compute unit, and the same 4 waves a SIMD the registers give; and it needs a sync and a second
  layout for the tile's edge column. The cache reads cost less than that.
- *Exact.* Each weight is a multiple of 1/128; with integer codes every product and partial sum is a
  multiple of 1/2048 below 2^24 of them, so float32 gives the exact value in any order (down then across
  here, across then down in Bale's): the pass, its model and Bale's stage have the same chroma. The C++
  check (`testing/display_pass`) has cases at both sitings, sharp chroma edges where the cubic overshoots
  and is clamped (limited and full range, 8 and 10 bits, every path), and the bilinear switch on every
  path.

**Registers** (RDNA2, LLPC, `testing/shader_regs`): PQ 110 VGPRs (114 before), 4 waves a SIMD as before;
SDR 96 and HLG 96, 4 waves (8 and 6 before). The SDR and HLG pipelines lose occupancy: the path that binds
is the 2:1 one at 16-bit samples (the four tap rows' 24 dwords and their 40 samples beside the tone map);
3:2 and the area loop are as large. Every read of a step is issued together and each step has ~400
instructions of arithmetic a wave behind it, so 4 waves are expected to hide the latency, but this is the
first thing to check on the console.

**Expected cost** (not measured; instructions counted from the code, at 0.93 T lane instructions a second):
about 23 instructions a source pixel more than bilinear at 2:1 (the filter down and across, the extraction
of 40 samples a row, the clamps): ~+0.2 ms a 4K frame (SDR 1.0 to ~1.2 ms, PQ 1.2 to ~1.4); at 1:1 and
3:2 ~+0.25-0.3 ms; the area loop (1080p to 4K, other ratios) ~+0.3-0.5 ms, its four tap rows one after
the other. Bale measured its own cubic chroma at ~1.1 ms a 4K frame in its D3D11 stage. Sized for the
GPU slice (1.86 TFLOP/s, never preempted): no memory, no CPU work, no more dispatches or syncs. The A/B
switches (`av1_set_display_bilinear_chroma`, `-displaybilinearchroma`) give version 2's image for the
picture, not its occupancy (one pipeline serves both).

## The block paths' chroma window (1:1 and 3:2)

Branch `exp/display-speed-opus` (2026-11-24). The console measured the Catmull-Rom chroma above at +0.2 /
+0.3 ms a 4K frame at 2:1 (SDR / PQ), as estimated, but +0.64 / +0.62 at 3:2 and +0.91 / +0.80 at 1:1,
the same with the bilinear weights: the cost was the structure. The RDNA2 listing (`testing/shader_regs`'
tools, one path compiled at a time) shows why: a block quad took its source rows one a step with the
four tap rows of each, 16 chroma reads a 1:1 quad and 24 a 3:2 one, the window rows re-read and re-extracted
for every source row, the five columns filtered down for every source row, and the compiler split each
step's reads into two or three rounds; 1054 instructions a 1:1 PQ quad a wave against 682 with version 2's
bilinear chroma (1775 against 1292 at 3:2). The image is unchanged (below).

**How it reads now** (`quad_block`, `display_pass.hlsl`):

- *One window a block.* The taps of a block's source rows (two at 1:1, three at 3:2) all lie in five
  chroma rows. Each row of the window is filtered across once, at the block's chroma columns (1:1: the
  block's column and the half-way one; 3:2: three of c0, c0 + 1/2, c0 + 1, c0 + 3/2 by the block's column
  parity), with the columns outside the plane folded into the weights; then the window is filtered down
  once a source pixel with the weights of its row's parity, and clamped. Across first: 2 or 3 values a
  window row instead of 5 columns filtered down for every source row.
- *A window rolls down the thread's quads.* A thread's four quads are blocks one below the other, and
  each window is the one above moved down by one row (1:1) or by two and one in turn (3:2: blocks of three
  rows). The loop over the quads carries the across values of the four rows the next window keeps (16
  floats at 1:1, 24 at 3:2), and a quad reads only its new rows: 1:1 one row of U and V, 3:2 two (after a
  move by one, one of them again). Per thread 8 window rows a plane for four 1:1 quads and 11 for four 3:2
  quads (32 and 48 before); the first quad and the edge step read all five. A quad's reads (its luma rows
  and its new chroma rows) are issued together, one round of latency.
- *A loop a path.* The flags are uniform, so `main` picks the path once and runs a loop over the quads
  for it (`tile`): one for each block path and sample size, one for 2:1 and the area loop. The window then
  costs the 2:1 path no registers, and the 8- and 16-bit block paths do not share one register allocation
  (in one loop PQ took 154 VGPRs). The 2:1 path compiles to the same instructions as before.
- *Registers, not group memory.* A group's chroma tile would read each sample once, but at 3:2 it is 21 KB
  (52 rows of 50 samples, U and V, 16 bits for each of the across values) on top of the 12.5 KB the pass
  has: two groups a compute unit, 2 waves a SIMD. The rolling window keeps 4 waves with no group memory and
  no sync. The decode of a 3:2 block is a loop over its three rows (unrolled: PQ 118 VGPRs); 1:1 is
  unrolled.

**Exact.** Every weight is a multiple of 1/128 and every code an integer below 1024, so every product and
partial sum is exact in float32 and any order gives the same chroma (across then down here, down then
across in interface version 3's first code); the light sums after the decode are in the same order as
before. `testing/display_pass/run.sh` proves it: besides holding the pass to its model, it builds the same
C++ check on the shader of `v0.8.0-alpha.1` (from git: `REFERENCE` in `run.sh`; CI's shallow checkout
fetches the tag) and requires every display image to be byte-identical on its 75 cases and on an identity
matrix of 64 more (`display_check --matrix`: each of 2:1, 1:1, 3:2 and the area loop with 8- and 10-bit
frames, NV12 and P010, the dither off and on, and both chroma switches; modes, sitings, ranges and content
going round): 139 cases identical. 45 mutations are rejected, 10 of them new (the window's rows, moves,
first quad and bottom edge, the folded edge columns, the siting of a 3:2 block's rows).

**Instructions a quad a wave** (RDNA2/LLPC listing of one path at a time, 16-bit samples; a thread's four
quads and its quarter of the edge step; in brackets interface version 3's first code, then version 2's
bilinear chroma):

| Path | PQ | SDR | HLG |
| --- | ---: | ---: | ---: |
| 1:1 | 789 (1054; 682) | 520 (741; 446) | 683 (896; 601) |
| 3:2 | 1474 (1775; 1292) | 970 (1272; 792) | 1343 (1611; 1149) |

A 1:1 quad that rolls takes 744 (PQ), a first one 896. The chroma's cost over version 2 falls from 372 to
107 instructions a 1:1 PQ quad and from 483 to 182 a 3:2 one.

**Registers** (`testing/shader_regs`): PQ 112 VGPRs (110 before), SDR 96, HLG 96: 4 waves a SIMD each, as
before. Each loop on its own (the pipeline with the other loops taken out; PQ / SDR / HLG): 2:1 and the
area loop 96 / 96 / 96, 1:1 16-bit 80 / 49 / 57, 1:1 8-bit 72 / 56 / 64, 3:2 16-bit 111 / 95 / 90, 3:2
8-bit 112 / 96 / 92. SDR and HLG would need both the 2:1 loop and the 3:2 loop under 80 for 6 waves; the
2:1 path is left as it is (it must not get slower), and pipelines a path would not help the 3:2 loop, whose
window is 24 of its registers. The console's +0.64 ms at SDR 3:2 in version 3 was its instruction count at
PQ's rate (both at 4 waves), so the instructions are the lever taken here.

**Expected** (the console's added cost of version 3's first code scaled by the instructions the chroma
now adds over version 2, which also takes the 4 waves into account): PQ 1:1 +0.23 ms (2.60 to ~2.0 ms a 4K
frame), PQ 3:2 +0.23 (2.13 to ~1.75), SDR 3:2 +0.24 (1.76 to ~1.35), SDR 1:1 +0.23 (2.42 to ~1.75; not a
case Bale runs); 2:1 and the area loop as before (1.23 / 1.49 ms at 1080p).
Sized for the GPU slice (1.86 TFLOP/s, never preempted: the pass costs its instructions), wave64, no
16-bit arithmetic, no group memory, no CPU work, no change to the interface.

**Measured** (console, idle state, the 4K60 60 Mbps clip, 600 frames, two rounds within 0.01 ms; v0.8 =
`91a4091`, the window = `2aaa9e7`), ms a 4K frame:

| Output | Path | SDR | PQ (HDR10) |
| --- | --- | --- | --- |
| 1920x1080 | 2:1 | 1.23 to 1.20 | 1.49 to **1.59** |
| 2560x1440 | 3:2 | 1.76 to 1.31 | 2.13 to 1.75 |
| 3840x2160 | 1:1 | 2.42 to 1.61 | 2.60 to 2.01 |

The block paths as expected; PQ at 2:1, the case Bale runs most (HDR10 on a 1080p display), 0.10 ms slower
(next section).

## Two pipelines a mode: the 2:1 path on its own

Branch `exp/display-speed-opus`, after the window above. **What the listings show** (RDNA2/LLPC, the whole
PQ pipeline of v0.8 and of the window, 16-bit samples, the case of the clip):

- The 2:1 loop did not change. Its four steps a quad are the same 566 instructions in both, instruction
  for instruction with the registers renamed: the same 9 reads a step (one luma read, 8 chroma), the same
  table reads in group memory, the same waits and the same exact-peak branches. The DXIL of its blocks is
  the same too (144 instructions, 10 reads, 12 group memory reads each).
- Around it a quad costs fewer instructions in the window's pipeline, not more: 178 against 192 outside the
  row loop (v0.8 tested the 1:1, 3:2 and quad flags in every step; `tile` tests them once a dispatch), and
  the 24 of the stores the same; the table load at the start (10) is the same, the prologue once a thread
  54 against 34.
- What did change is the pipeline around the loop: its machine code from 36.5 to 50.4 KB (+38%), its DXIL
  from 10298 to 13730 instructions, five loops over the quads (one a path) with 7 barriers instead of one
  loop with 3, VGPRs 110 to 112, and **SGPRs 98 to 106, the ceiling of RDNA2** (`AVAILABLE_SGPRs` in RGA's
  analysis). PQ is the only mode that reached it (SDR 95, HLG 103), and the only one that got slower at 2:1.
  No spills, no scratch, 4 waves a SIMD in both.

So the cause is not in the 2:1 path's code as LLPC compiles it: LLPC gives the window's PQ pipeline the
same 2:1 loop and a shorter quad. The console compiles the DXIL with its own compiler, whose listing there
is no way to see; what the listings do show is what that compiler was given: the same 2:1 code inside a
pipeline that is 38% larger and sits at the scalar register ceiling for PQ alone. The change most likely
to give the 2:1 path its cost of v0.8 back whatever that compiler does with it is to take the block paths
out of its pipeline.

**The change.** Two pipelines a mode (`DISPLAY_PATHS`, `display_pass.hlsl`): `display_pass`,
`display_pass_sdr`, `display_pass_hlg` have the 2:1 path and the area loop (`tile(0)`), and
`display_pass_block`, `display_pass_block_sdr`, `display_pass_block_hlg` (a define and an include each) the
block paths (`tile(1..4)`). The host takes the block pipeline of the frame's mode for a frame whose flags
have bit 8 or 9 (`display_pass::block_path`, the flags it set itself), the other for the rest: an internal
change, no change to `rav1g.h`. The C++ check picks the paths by the same rule (`paths()`), so the identity
check runs each pipeline's own `main`. RDNA2/LLPC (`testing/shader_regs`):

| Pipeline | VGPRs | SGPRs | Code | Waves a SIMD |
| --- | ---: | ---: | ---: | ---: |
| `display_pass` (PQ, 2:1 and area) | 96 (112) | 98 (106) | 23.6 KB (50.4) | 4 |
| `display_pass_sdr` | 96 (96) | 87 (95) | 18.0 KB (42.0) | 4 |
| `display_pass_hlg` | 96 (96) | 95 (103) | 22.1 KB (47.4) | 4 |
| `display_pass_block` (PQ, 1:1 and 3:2) | 112 | 75 | 27.0 KB | 4 |
| `display_pass_block_sdr` | 96 | 62 | 24.1 KB | 4 |
| `display_pass_block_hlg` | 93 | 70 | 25.5 KB | 4 |

In brackets the one pipeline a mode of the window. The PQ 2:1 pipeline is v0.8's in what runs: the same 566
instructions a quad's row loop, 182 around it (192 in v0.8), the prologue of v0.8 (34), SGPRs 98 as in v0.8,
and 14 fewer VGPRs (96, 110 in v0.8; 4 waves a SIMD either way for a group of 256 threads in CU mode, 5
in WGP mode). The block pipelines keep the window's loops (123 of their 130 blocks the same instructions; a
3:2 16-bit quad 5 scalar instructions more). 96 VGPRs is the edge of 5 waves a SIMD (in WGP mode): the
2:1 loop alone takes 91, the area loop 96.

**What it costs.** Three pipelines more (109 against 106, made on the creation threads at the start like
the others). The machine code is the same in total (140.3 KB for the six against 139.8 KB for the three:
each path's code is in one pipeline either way), so the pipeline-state memory grows by the fixed part of
three pipeline objects, a few KB each; the embedded DXIL by 16.6 KB (295 against 278 KB). One pipeline
switch a frame as before (the pass is one dispatch). Sized for the GPU slice (1.86 TFLOP/s, never
preempted) and the per-frame budget at 60 fps: the 2:1 path is the case Bale runs for HDR10 on a 1080p
display, where every 0.1 ms is 0.7% of the 14 ms rav1g has a frame; memory: nothing against the 1.6 GB
envelope.

**Expected** (to be measured on the console): PQ 2:1 back to v0.8's 1.49 ms or below (from 1.59), SDR 2:1
1.20 or below; PQ 3:2 1.75 and 1:1 2.01, SDR 3:2 1.31 and 1:1 1.61 as measured with the window. The image is
unchanged: `testing/display_pass/run.sh` finds all 139 cases byte-identical to `v0.8.0-alpha.1`.

## The ABI it needs

What follows is the first sketch; the draft of `exp/display-abi2-opus` is in `rav1g.h` (interface version 2,
fingerprint `d754ced4ce571264`) and [integration.md](integration.md#the-display-pass-interface-version-2).
It differs from the sketch in: `transform` is `RAV1G_DISPLAY_TRANSFORM_AUTO` or a transform forced for
every frame; `Rav1gFrame::display_transform` says which one a frame took (in place of a boolean); the
tone curve's 0 is BT.2390 and Hable is refused; `depth` 0 follows the decoder's bit depth; NV12 is
rounded, not dithered.

For Bale to use it (`RAV1G_ABI_VERSION` 1 to 2, `tools/rav1g-header/abi.txt`, `docs/integration.md`):

```c
/* rav1g.h, new. */
#define RAV1G_DISPLAY_SCALE 1     /* BT.709 SDR: downscale only */
#define RAV1G_DISPLAY_TONE_MAP 2  /* PQ, HLG, BT.2020 SDR to BT.709 SDR; BT.709 SDR downscaled */
#define RAV1G_TONE_CURVE_BT2390 1
#define RAV1G_TONE_CURVE_HABLE 2

typedef struct Rav1gDisplay {
  uint32_t width, height;          /* the image size, even, at most 4:1 per axis; 0, 0: no display pass */
  uint32_t transform;              /* RAV1G_DISPLAY_* */
  uint32_t tone_curve;             /* RAV1G_TONE_CURVE_* */
  float source_peak_nits;          /* 1000: Bale's server default */
  float target_white_nits;         /* 100: the 203-nit endpoint of the EETF */
  float desaturation;              /* 0 */
  uint32_t depth;                  /* 10: P010; 8: NV12 with the 8x8 Bayer dither (an 8-bit display) */
  uint8_t assumed_transfer;        /* H.273 code points used where the sequence header says   */
  uint8_t assumed_primaries;       /* "unspecified" (2), as the fork merges the container's tags; */
  uint8_t assumed_matrix;          /* 0: none                                                  */
  uint8_t assumed_chroma_position; /* AV1 chroma_sample_position */
  uint32_t reserved[4];            /* zero */
} Rav1gDisplay;

/* Rav1gSettings, appended behind `output`: */
  struct Rav1gDisplay display;     /* rav1g_settings_default: all zero */

/* Rav1gFrame, appended behind `format`: */
  bool display_processed;          /* the image is the display pass's; `color` is its BT.709 SDR */
  uint32_t source_width, source_height;
  struct Rav1gColorInfo source_color;
```

- **Images**: with a display size, `Rav1gImageRequest::width`/`height` are the display size and
  `format` is P010 (depth 10) or NV12 (depth 8) whatever the stream's depth; the image must be a
  texture with `D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS` (no staged path: a copy would give back
  what the pass saves); anything else fails the temporal unit with an `OUTPUT` line, as today.
- **Frames**: `width`/`height`/`format`/`bit_depth` describe the image; `color` is BT.709, limited
  range, left-sited; the HDR10 metadata stays the stream's (for the stats), `source_*` say what was
  decoded.
- `rav1g_settings_default` leaves `display` zeroed, so an application built against ABI 2 that does not
  set it gets today's output. Bale's loader compares the ABI version; the fork fills `display` from the
  `BaleToneMapConfig` it builds today and skips `BaleToneMap::Process` for such decoders.
