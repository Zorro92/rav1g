# Adopting rav1g's display pass in Bale and the fork

A draft (2026-11-23, branch `exp/display-v2-opus` on `exp/display-abi2-opus`, not released): what Bale and
its FFmpegInteropX fork change to let rav1g write the display-size, tone-mapped image that Bale's D3D11
stage writes today for AV1, and what they keep. The interface is `Rav1gSettings::display` of interface
version 2 ([integration.md](integration.md#the-display-pass-interface-version-2)); the design and the
measurements are in [stage-fusion.md](stage-fusion.md). Nothing here is decided: the owner decides on the
interface and its release.

The interface is not Bale's: a display size, a tone map with its two luminances, an output depth and
dither, and the colour code points an application merges from its container. Another application (or a
later backend of rav1g) sets them or leaves `display` zero, and then gets the plain decoder of interface
version 1.

- [When the stage runs, and the sizes](#when-the-stage-runs-and-the-sizes)
- [Bale's default configuration, and what rav1g takes](#bales-default-configuration-and-what-rav1g-takes)
- [How close to Bale's stage](#how-close-to-bales-stage)
- [What goes away for AV1](#what-goes-away-for-av1)
- [What the fork changes](#what-the-fork-changes)
- [What Bale changes](#what-bale-changes)
- [What Bale and the fork keep](#what-bale-and-the-fork-keep)
- [Costs and savings](#costs-and-savings)

## When the stage runs, and the sizes

Bale's stage runs for (a) an HDR source on an SDR display, tone-mapped at any size, 1:1 included (a 4K SDR
TV showing HDR10), and (b) an SDR source larger than the display, downscaled only. On an HDR display the
video passes through, with no stage. The display is the console's HDMI mode: 1920x1080 or 3840x2160 in
practice, 2560x1440 possible; the size is the source fitted into it, aspect kept, even
(`VideoFrameProcessor::SelectOutputSize`; a source that fits keeps its size).

| Source | Display | Image | rav1g's path | Expected, PQ (SDR) |
| --- | --- | --- | --- | --- |
| 3840x2160 | 1920x1080 | 1920x1080 | 2:1 | 1.59 ms (1.20) measured |
| 3840x2160 | 3840x2160 | 3840x2160 | 1:1 | 2.01 ms measured (BT.709 SDR: no stage) |
| 3840x2160 | 2560x1440 | 2560x1440 | 3:2 | 1.75 ms (1.31) measured |
| 1920x1080 | 3840x2160 or 1920x1080 | 1920x1080 | 1:1 | ~0.6 ms (no stage for SDR) |
| 3840x1600 (2.4:1) | 1920x1080 | 1920x800 | 2:1 | ~0.86 ms |
| 3840x1600 | 2560x1440 | 2560x1066 | area loop (not exactly 3:2 vertically) | slower |

Times are a 4K 10-bit frame on the console in the idle state, interface version 3 (Catmull-Rom input
chroma; the measured rows on a 4K60 60 Mbps clip, two rounds; the others expected from the instructions a
quad of each path, [stage-fusion.md](stage-fusion.md#11-32-and-the-dither)); HLG is about 15-25% above PQ.
Version 2's bilinear chroma took 1.17 (1.01), 1.80 and 1.51 (1.12) ms in the three measured rows;
`v0.8.0-alpha.1`, before the block paths' chroma window, 1.49 (1.23), 2.60 and 2.13 (1.76).

## Bale's default configuration, and what rav1g takes

From the fork's `BaleToneMap` (`BaleToneMap.h`: `Create`, `Initialize`, `Process`) and `bale_tonemap.h`:

| Bale's stage | Bale's default | `Rav1gDisplay` | |
| --- | --- | --- | --- |
| curve 1, BT.2390 EETF (knee offset 1, the largest original R'G'B' signal, a scalar gain) | yes | `tone_curve` 0 or `RAV1G_TONE_CURVE_BT2390` | the same |
| curve 2, Hable | debug switch only | `RAV1G_TONE_CURVE_HABLE` | refused, `RAV1G_STATUS_INCAPABLE`: the fork keeps its stage |
| `source_peak_nits` | 1000 (HLG too): the PQ peak without metadata | `source_peak_nits` 0 or 1000 | the same; HLG's OOTF at a 1000-nit display, as Bale |
| PQ source peak from the HDR metadata (`cpu::source_peak`, since 2026-11-24) | MaxCLL, else the mastering maximum, the smaller when MaxCLL is above it, clamped to 400..10000; else the config peak | the stream's CLL/MDCV OBUs, `max_cll_nits` and `mastering_max_nits` where the stream has none | the same rule (interface version 3); `Rav1gFrame::display_source_peak_nits` reports it; `fixedpeak`: `av1_set_display_fixed_peak` |
| `target_white_nits` | 100: the EETF ends at 2.03 x 100 = 203 nits, normalized to SDR 1; the fork will pass 80 to 400 | `target_white_nits` 0 or 80..400 | the same (not 203: 203 would end the curve at 412 nits) |
| `desat` | 0 (off) | `desaturation` 0 | the same; any other value is refused (`INCAPABLE`) |
| downscale (`reserved[0..1]`, at most 4:1) | the source fitted into the display | `width`, `height` | any ratio; 2:1, 1:1 and 3:2 fast |
| output P010 (`BALE_TONEMAP_OUTPUT_P010`) | P010 for AV1 (the owner's exception) | `depth` 10 (or 0 for a 10-bit decoder) | the same codes: `64 + 876 Y'`, `512 + 896 C`, rounded, `<< 6` |
| output NV12 dithered (`BALE_TONEMAP_OUTPUT_NV12_DITHERED`) | 8-bit SDR displays, AV1 included since Bale lifted its P010 exception (2026-11-26) | `depth` 8, `dither` 0 | dithered with the same R2 threshold: Bale's stage took rav1g's (2026-11-26, no cost against its 8x8 Bayer on the console) |
| BT.709 SDR scale only (`BALE_TONEMAP_SDR709`) | SDR larger than the display | transform auto | the same: BT.1886 decode, area, encode, no clip |
| BT.2020 SDR, gamut only | yes | transform auto | the same |
| 8-bit input (`BALE_TONEMAP_INPUT_NV12`, read as code << 2) | yes | 8-bit sequences | limited range the same (8-bit codes by their 8-bit range equal `code << 2` in the 10-bit one); full range by 255 (H.273), where `code << 2` over 1023 is 0.3% lower at the top |
| frame transfer, primaries, matrix, range | the stream's, unspecified merged from the container | the sequence header's; `assumed_*` where it says unspecified (2) | the same, fill only the unspecified values |
| input chroma siting (`chroma_location`, AVChromaLocation) | the stream's: left and unspecified as left, topleft co-sited, center/top/bottom honoured | the sequence header's `chroma_sample_position`, `assumed_chroma_sample_position` where it is 0 | the same for what AV1 can say: vertical (left) and colocated (topleft); unknown as left. Center, top and bottom have no AV1 code point |
| input chroma filter (since 2026-11-24) | 4x4 Catmull-Rom, clamped to 64..960 limited (0..1023 full); `bilinearchroma`: bilinear | the same cubic and clamp (interface version 3); `av1_set_display_bilinear_chroma`: bilinear | the same chroma values: both exact at the quarter phases |
| mastering display, content light | the PQ source peak (above) | the PQ source peak (above) | the same (the frame's metadata stays the stream's) |
| gamut step | `matrix == 9` | `primaries == 9` | the same for every stream Bale's stage takes (it declines mixed BT.2020/BT.709 tags) |
| `gammachroma`, `nostaged` (debug) | off | none | Bale's old math and its other pass layout: no equivalent |

So Bale's default configuration needs, of what it can hand to rav1g: BT.2390 with 1000 and 100 nits, no
desaturation, P010 (NV12 dithered if the AV1 exception is ever lifted), the scale-only and gamut-only SDR
transforms, 8-bit sources, the container's code points for unspecified ones, the stream's chroma siting.
All of it is in this version; Hable and desaturation are refused so that the fork falls back.

The method, point by point as Bale's stage does it since 2026-11-24: linear-light area downscale with
exact overlap weights, each output pixel computed once; input chroma upsampled with a 4x4 Catmull-Rom
cubic at the stream's siting, clamped to the code range; the PQ source peak from the HDR metadata; per
source pixel the sign-keeping decode, the tone curve on the largest original channel and
BT.2020 to BT.709, unclipped; one clip at the end, per output pixel and per chroma footprint; output
chroma left-sited, mixed in linear light over [1 2 1]/4 x [1 1]/2 (output columns 2i-1..2i+1, rows 2j,
2j+1), clipped and encoded once; inverse BT.1886 (power 1/2.4). rav1g's pass does each of these, written
from the standards (ST 2084, BT.2100, BT.2390, BT.2087, BT.709/BT.1886) and held to its own float32 model
(`testing/display_pass/model.py`); no code of Bale's stage is in rav1g.

## How close to Bale's stage

Bale's CPU reference of its stage (`cpu::reference_p010` of `native/bale-gpu`, Bale's default
configuration) and rav1g's pass (the shader compiled as C++, `testing/display_pass`), on the test frames of
the C++ check (gradients, flat blocks, noise, samples out of the nominal range, every code at random,
near-black out-of-gamut blocks), P010 (run locally on 2026-11-23, not part of CI; Bale's crate is
AGPL-3.0 and is only run, not copied). These numbers are of interface version 2 against Bale's stage of
that day (bilinear chroma, the fixed peak); the comparison has not been run again for version 3 and Bale's
stage of 2026-11-24. Both now take the same cubic chroma, exact at its quarter phases on both sides, and
the same peak rule, so the same agreement is expected; with `av1_set_display_bilinear_chroma` and
`av1_set_display_fixed_peak` the pass is version 2's:

| Cases | Codes that differ (largest share of a plane) | Largest difference |
| --- | --- | --- |
| 2:1, PQ, HLG, BT.2020 SDR, BT.709 SDR, full range, colocated, random codes, near black | 0.22% | 1 |
| 1:1, the same | 0.29% | 1 |
| 3:2 (block path and area loop), the same | 0.50% | 1 |
| BT.709 SDR and BT.2020 SDR on every path | 0.04% | 1 (mostly 0) |

The codes that differ are the same ones that differ between the pass and rav1g's model: Bale's reference
and rav1g's model agree with each other, and the pass is one float32 rounding away from both. Bale holds
its own GPU stage to its reference within 12 P010 codes (3 NV12). Not comparable: odd sizes, upscaling
and BT.601 (Bale's reference declines them, rav1g takes them), and dithered NV12 before
2026-11-26 (Bale's stage had an 8x8 Bayer threshold; it has rav1g's R2 threshold since).

## What goes away for AV1

On the rav1g path (`GpuDecoderSampleProvider` with a `BaleRav1gDecoder`) and only there; HEVC, H.264, VP9
and the FFmpeg/D3D11VA AV1 paths keep the stage as it is.

- **The stage's draws** (`BaleToneMap::Process`: at a downscale the area pass into the fp32 intermediate,
  the luma pass and the chroma pass; at 1:1 the luma and the chroma pass) and the cross-API wait in front
  of them for each frame.
- **The intermediate** (`R32G32B32A32_FLOAT` at the output size, 31.6 MiB at 1080p) and the stage's own
  resources for rav1g streams (`bale_tonemap_create` is not called for them).
- **rav1g's 4K images** on a smaller display: the image pool of a rav1g decoder holds display-size images
  (4 x 5.9 MiB P010 at 1080p, 4 x 10.5 MiB at 1440p, instead of 4 x 23.7 MiB at 3840x2160). At 1:1 the
  images stay 4K.
- **The image hold**: the fork copies the image into a texture of its pool and gives it back at once,
  where today the image is held until the stage's draws have read it (the 13.7 ms a frame rav1g waits for
  an image in HDR10 at 60 Mbps).
- **rav1g's own copy pass** of frames without a postfilter stage (38% of the frames of the 60 Mbps HDR10
  clip): the display pass reads the frame buffer and writes the image, nothing else is written.

## What the fork changes

`BaleRav1g.h`, `GpuDecoderSampleProvider.h`, `GpuDecoderImagePool.h`, `FFmpegMediaSource.cpp` (the rav1g
branch), and its copies of `rav1g.h` and `av1_perf.h`:

1. **Interface version.** The new `rav1g.h`, `static_assert(RAV1G_ABI_VERSION == 3, ...)` and the
   run-time check of `rav1g_abi_version()` against 3 ([integration.md](integration.md#interface-version-3-the-retired-switches-the-source-peak-and-the-input-chroma):
   the retired switches, which the fork does not call; `Rav1gDisplay::max_cll_nits` and
   `mastering_max_nits` in two of its reserved words, `Rav1gFrame::display_source_peak_nits` at its end).
   Bale's loader refuses a version-1 DLL, or the fork keeps a version-1 path (no display pass, the stage
   as today) for one.
2. **`av1_perf.h`**, copied verbatim as today: `av1_perf_frame` has one GPU stage more
   (`AV1_PERF_GPU_DISPLAY`, so `gpu_stage_ms` and `gpu_stage_hits` have 22 entries) and the output path
   `AV1_PERF_OUTPUT_DISPLAY`; the record is 640 bytes on x64 (the fork's `static_assert` of its size
   changes with it). `user_data` stays.
3. **Settings** (`BaleRav1gDecoder`, where it fills `Rav1gSettings` today): when the stage would run for
   the stream (the condition of `FFmpegMediaSource.cpp` around `BaleToneMap::Create`: HDR on an SDR
   display, or SDR larger than the target), fill `settings.display` instead of creating a `BaleToneMap`:
   - `width`, `height`: what `SelectOutputSize` gives (at 1:1 the source size);
   - `tone_curve`: `RAV1G_TONE_CURVE_BT2390` for Bale's curve 1; curve 2 (Hable) or a non-zero
     desaturation: `rav1g_decoder_create` returns `RAV1G_STATUS_INCAPABLE`, and the fork creates the
     decoder again without `display` and keeps the stage;
   - `source_peak_nits` 1000, `target_white_nits` 100, or the fork's 80 to 400 (the `BaleToneMapConfig`
     values; 0 is 1000 and 100);
   - `max_cll_nits`, `mastering_max_nits`: the container's MaxCLL and mastering display maximum (what the
     fork fills `BaleToneMapFrameInfo` with: `has_content_light`/`max_cll`, `has_mastering`/
     `mastering_max_nits`), 0 where it has none. rav1g takes the stream's metadata OBUs first and these
     where the stream says nothing, then Bale's rule; the fork can log `frame.display_source_peak_nits`
     as Bale's `bale_tonemap_peak_read` reports its stage's;
   - `depth`: 10 for P010 (Bale's rule for AV1), 8 for NV12; `dither` 0 (NV12 dithered, P010 rounded);
   - `assumed_primaries`, `assumed_transfer_characteristics`, `assumed_matrix_coefficients`: the
     container's code points (`streamColor` of `MergeStreamColor`; AVCOL_* are H.273 numbers), used only
     where the sequence header says unspecified (2), as `MergeStreamColor` merges today. The fork's rule
     that unspecified tags count as BT.709 only for video of 720p and larger stays the fork's: it decides
     whether to set `display` at all;
   - `assumed_chroma_sample_position`: from the container's `AVChromaLocation`, `AVCHROMA_LOC_LEFT` (1) to
     1 and `AVCHROMA_LOC_TOPLEFT` (3) to 2; 0 for the others;
   - `transform`: `RAV1G_DISPLAY_TRANSFORM_AUTO` (0): each frame by its colour description, so a new
     sequence with another transfer needs nothing from the fork.
4. **The image pool** (`GpuDecoderImagePool.h`): images of `request->width` x `request->height` as today,
   which are now the display size; P010 (or NV12) by `request->format`; with `D3D11_BIND_UNORDERED_ACCESS`,
   which the pool asks for already. A pool that falls back to textures without unordered access cannot
   have the display pass (rav1g fails the temporal unit with a "DISPLAY" log line): create the decoder
   without `display` then.
5. **`OutputFrame`** (`GpuDecoderSampleProvider.h`):
   - the check of the frame's size against the stream's takes `frame.source_width` and
     `frame.source_height` (`width`/`height` are the image's: the display size);
   - `MergeStreamColor` takes `frame.source_color` for what the stream said (the stats, the sample's
     attributes); the sample's colour is `frame.color`, BT.709 SDR limited range, chroma left, as the
     stage's output is today;
   - when `frame.display_transform` is not `RAV1G_DISPLAY_TRANSFORM_NONE`: `CopyResource` of the image
     into a texture of `processedTexturePool` (the stage's output size and format) in place of
     `frameProcessor->Process`. The copy stays because the player keeps samples and must never hold
     decoder images (the deadlock the comment there describes). It is cheap at 1080p (6.2 MB) and costs
     ~1.2 ms at 1:1 4K (24.9 MB read and written at the idle state's copy rate); a later step can let
     rav1g write the player's textures directly;
   - `ToneMappedOutputMark`, the media type at the display size and the fallback to `BaleToneMap` stay.
6. **The stats** (`BaleRav1gPerf::OnFrame`): the window sums `gpu_stage_ms[AV1_PERF_GPU_DISPLAY]` of the
   frames that have it (`gpu_stage_hits`) as the display stage's GPU time, and the per-frame line can
   print it next to `gpu`; `output_path` is `AV1_PERF_OUTPUT_DISPLAY` (6) for such frames.
7. **A switch for the A/B** (an FFmpeg option next to `bale_tonemap`): create the rav1g decoder without
   `display` and keep Bale's stage, so the same stream can be played both ways. `tonemap-debug
   fixedpeak` and `bilinearchroma` map to `av1_set_display_fixed_peak(1)` and
   `av1_set_display_bilinear_chroma(1)` of `av1_perf.h` (process-wide, set before the first temporal
   unit), so that both paths A/B the same way.

## What Bale changes

- **The stats pane**: "Stage GPU" for an AV1 stream on the rav1g path is the display stage of the perf
  record (item 6 above), not the stage's own timestamps (`bale_tonemap_debug_timing_read`, which has no
  stage to measure then); "Stage submit to GPU done" goes away for it.
- **The debug switch** that forces Bale's stage (item 7) for the A/B, in the same debug surface as the
  stage's other switches.
- Nothing in the playback policy: when to tone map, the display mode, the output depth rule and the
  route are Bale's as today; rav1g only takes the result of those decisions.

## What Bale and the fork keep

- **The stage** for every other codec, for AV1 through FFmpeg/D3D11VA, for Hable and desaturation, and for
  a rav1g DLL of interface version 1.
- **The choice of the display size and the target**, which rav1g takes as given. A display mode change
  recreates the decoder (as a new sequence that needs `RAV1G_STATUS_NEEDS_RECONFIGURE` does).
- **The colour attributes of the sample**, from `Rav1gFrame::color` (BT.709 SDR) and the HDR metadata
  (`mastering_display`, `content_light`, which stay the stream's for the stats).
- **The copy into the fork's pool** (above), and the processed-output mark.
- **One math in two places**: rav1g's pass and Bale's stage are written separately from the same
  standards and agree within one P010 code (above); a change of the method (another curve; the source
  peak from the metadata and the cubic chroma were made in both, 2026-11-24) has to be made in both, or
  Bale keeps its stage for it.

## Costs and savings

Per 4K60 HDR10 frame, rav1g's queue and Bale's on the console's one GPU
([stage-fusion.md](stage-fusion.md#what-each-saves) for the bytes):

| Display | Today: rav1g 4K image + Bale's stage | rav1g display pass + the fork's copy |
| --- | --- | --- |
| 1920x1080 | output fused into the last stage (~0.2 ms) or the 1.10 ms copy; the stage up to ~5.6 ms SDR, ~9 ms HDR10 with the renderer (Bale's logs); ~156 MB moved, 3 draws, a cross-API wait | 1.19 ms measured (PQ; 1.01 SDR); the copy of 6.2 MB; ~31 MB moved |
| 2560x1440 | the same as 1080p with a 2560x1440 intermediate | ~1.8 ms (PQ), the copy of 11 MB |
| 3840x2160 (SDR TV) | the 4K output, and the stage's two draws: each source pixel evaluated once for luma and six times a chroma sample (20.7 M evaluations, what took ~10 ms at 2:1 before Bale's restructuring) | ~2.45 ms (PQ), the copy of 24.9 MB (~1.2 ms) |

Memory: at 1080p 4 x 5.9 MiB of images instead of 4 x 23.7 MiB and the 31.6 MiB intermediate; at 1:1
the images stay 4K, and Bale's stage has no intermediate there either. Measured on the console before this
branch (test player, idle state): 4K60 HDR10 at 60 Mbps plays at ~53 fps in Bale with the stage; the pass
at 1080p was 1.01 ms (SDR), 1.19 ms (PQ), 1.29 ms on a 4K50 stream, rav1g's GPU busy +0.55..0.8 ms against
its plain 4K output.
