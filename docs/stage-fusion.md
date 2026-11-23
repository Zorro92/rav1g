# Doing Bale's display stage in rav1g's last pass

A design study (2026-11-23): can rav1g's last pass write the display-size, tone-mapped image that
Bale's D3D11 stage makes today from rav1g's 4K output, and what would that save? No code of this
page is in the decoder yet.

- [Conclusion](#conclusion)
- [What Bale's stage computes today](#what-bales-stage-computes-today)
- [Bytes and passes of a 4K60 frame today](#bytes-and-passes-of-a-4k60-frame-today)
- [Three ways to fuse](#three-ways-to-fuse)
- [What each saves](#what-each-saves)
- [Interface](#interface)
- [What Bale and the fork would change](#what-bale-and-the-fork-would-change)
- [Risks](#risks)
- [Prototype: not built in this round](#prototype-not-built-in-this-round)

## Conclusion

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
  meaning of the output image are new settings), and **the tone-map math would have to be licensed
  into rav1g** (Bale is AGPL-3.0, rav1g BSD-2-Clause): both are the owner's decisions. So this round
  stops at the design, as the brief asks when an ABI change is needed. A prototype behind an internal
  switch is sketched at the end, for a round the owner approves.
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

An outline for an ABI change (`RAV1G_ABI_VERSION` 2; `abi.txt`, `docs/integration.md`):

- **Settings** (`Rav1gSettings`, appended): `display_width`, `display_height` (0: no display pass, the
  decoder's output as today); `display_transform`: `NONE` (scale only, BT.709 SDR), `TONE_MAP` (PQ,
  HLG and BT.2020 SDR to BT.709 SDR); `tone_curve` (BT.2390, Hable), `source_peak_nits`,
  `target_white_nits`, `desaturation`; `display_depth` (10: P010; 8: NV12 with the Bayer dither);
  `display_flags` (legacy math for A/B). The colour the application wants assumed where the stream
  says "unspecified" (the fork merges container tags, `MergeStreamColor`): `assumed_transfer`,
  `assumed_primaries`, `assumed_matrix`, `assumed_range`, `assumed_chroma_position`.
- **Images**: with a display size, `Rav1gImageRequest` asks for `display_width x display_height` (even)
  in the output format, whatever the stream's size and depth; a texture with
  `D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS` (the direct path only: a staged copy would give back
  what B saves). `Rav1gFrame::width`/`height` become the display size, `Rav1gFrame::color` the output's
  description (BT.709, limited range, left-sited), with the stream's own description in new fields
  (`source_color`, `source_width`, `source_height`) and a flag that the frame was processed (what the
  fork's `ToneMappedOutputMark` says to the player).
- **Per frame**: nothing from the application; rav1g has the frame's colour description and HDR10
  metadata (metadata OBUs) itself. A setter to change the target (display mode change) between
  frames would need a decoder flush; recreating the decoder is the simple first form.
- Internal A/B switches stay out of the ABI (`av1_*` in `av1_perf.h`).

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

## Prototype: not built in this round

The brief asks for a prototype only when the design shows a clear saving and needs no ABI change. The
saving is clear, but the feature needs an ABI change (display size, tone-map parameters, the meaning of
the output image) and the math would have to come from an AGPL-3.0 file into a BSD-2-Clause library:
both are for the owner to decide. What a prototype for one round would be, once approved:

1. `av1_set_display_pass(width, height, flags)` (internal, `av1_perf.h`), the test player's and
   rav1g-dec's `-displaypass WxH`; the output image is requested at that size, P010, direct path only.
2. `display_pass.hlsl`: option B's pass with the area downscale and the BT.709 SDR scale-only and PQ
   BT.2390 paths, 32x16 output tiles with the one-pixel ring in group shared memory.
3. rav1g-dec holds the image against a CPU model of the same math (a port of the needed functions of
   `cpu.rs`) within Bale's tolerances, on WARP in CI; the per-frame md5 stays on the frame buffer.
4. Console: `-gputime` with and without `-displaypass 1920x1080` on the 40 and 60 Mbps clips, and
   Bale's "Stage GPU" on the same clips for the other side of the sum.

Before that, option A is the cheaper first step and is Bale's alone.
