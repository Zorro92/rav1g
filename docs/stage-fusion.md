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
