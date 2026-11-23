# Adopting rav1g's display pass in Bale and the fork

A draft (2026-11-23, branch `exp/display-abi2-opus`, not released): what Bale and its FFmpegInteropX fork
would change to let rav1g write the display-size, tone-mapped image that Bale's D3D11 stage writes today
for AV1. The interface is `Rav1gSettings::display` of interface version 2
([integration.md](integration.md#the-display-pass-interface-version-2)); the design and the measurements
are in [stage-fusion.md](stage-fusion.md). Nothing here is decided: the owner decides on the interface.

## What goes away for AV1

On the rav1g path (`GpuDecoderSampleProvider` with a `BaleRav1gDecoder`) and only there; HEVC, H.264, VP9
and the FFmpeg/D3D11VA AV1 paths keep the stage as it is.

- **The stage's three draws a frame** (`BaleToneMap::Process`: the area pass into the 1920x1080 fp32
  intermediate, the luma pass, the chroma pass) and the cross-API wait in front of them for each frame.
- **The intermediate** (`R32G32B32A32_FLOAT` at the output size, 31.6 MiB at 1080p) and the stage's
  own resources for rav1g streams (`bale_tonemap_create` is not called for them).
- **rav1g's 4K images**: the image pool of a rav1g decoder holds display-size images (4 x 5.9 MiB P010 at
  1080p instead of 4 x 23.7 MiB at 3840x2160): about -71 MiB, plus the intermediate's 31.6 MiB.
- **The image hold**: the fork copies a 1080p image (6.2 MB at P010) into a texture of its pool and gives
  the image back at once, where today the 4K image is held until the stage's three draws have read it
  (the 13.7 ms a frame rav1g waits for an image in HDR10 at 60 Mbps).
- **rav1g's own copy pass** of frames without a postfilter stage (38% of the frames of the 60 Mbps HDR10
  clip): the display pass reads the frame buffer and writes the image, nothing else is written.

## What the fork changes

`BaleRav1g.h`, `GpuDecoderSampleProvider.h`, `FFmpegMediaSource.cpp` (the rav1g branch):

1. **Interface version.** `static_assert(RAV1G_ABI_VERSION == 2, ...)` and the run-time check of
   `rav1g_abi_version()` against 2, with the new `rav1g.h`. Bale's loader refuses a version-1 DLL, or the
   fork keeps a version-1 path (no display pass, the stage as today) for one.
2. **Settings** (`BaleRav1g.h`, where it fills `Rav1gSettings` today): when the stage would run for the
   stream (the condition of `FFmpegMediaSource.cpp` around `BaleToneMap::Create`: HDR, or SDR larger
   than the target), fill `settings.display` instead of creating a `BaleToneMap`:
   - `width`, `height`: what `VideoFrameProcessor::SelectOutputSize` gives today (the source fitted into
     the target, aspect kept, even, at most 4:1 per axis); rav1g takes any size and ratio.
   - `tone_curve`: `RAV1G_TONE_CURVE_BT2390` for Bale's curve 1. Hable (2) and a non-zero desaturation are
     not in this version: `rav1g_decoder_create` returns `RAV1G_STATUS_INCAPABLE`, and the fork creates
     the decoder again without `display` and keeps the stage.
   - `source_peak_nits`, `target_white_nits`: the `BaleToneMapConfig` values (1000 and 100 by default;
     0 is the same).
   - `depth`: 10 for P010 (Bale's rule for AV1), 8 for NV12. NV12 from rav1g is rounded, not dithered
     (see "What Bale and the fork still do").
   - `assumed_primaries`, `assumed_transfer_characteristics`, `assumed_matrix_coefficients`: the
     container's code points (`streamColor` of `MergeStreamColor`, AVCOL_* are H.273 numbers), used only
     where the sequence header says unspecified (2), as `MergeStreamColor` merges today.
     `assumed_chroma_sample_position`: from the container's `AVChromaLocation`, `AVCHROMA_LOC_LEFT` (1)
     to 1 and `AVCHROMA_LOC_TOPLEFT` (3) to 2; 0 for the others (an AV1 sequence header can only say
     these two, or unknown).
   - `transform`: `RAV1G_DISPLAY_TRANSFORM_AUTO` (0). Each frame is tone-mapped by its own colour
     description (PQ, HLG, BT.2020 SDR gamut, BT.709 SDR scaled only), so a new sequence with another
     transfer needs nothing from the fork.
3. **The image pool** (`GpuDecoderImagePool.h`): images of `request->width` x `request->height` as
   today, which are now the display size; P010 (or NV12) by `request->format`; with
   `D3D11_BIND_UNORDERED_ACCESS`, which the pool asks for already. A pool that falls back to textures
   without unordered access cannot have the display pass (rav1g fails the temporal unit with a "DISPLAY"
   log line): create the decoder without `display` then.
4. **`OutputFrame`** (`GpuDecoderSampleProvider.h`):
   - the check of the frame's size against the stream's takes `frame.source_width` and
     `frame.source_height` (`width`/`height` are the image's: the display size);
   - `MergeStreamColor` takes `frame.source_color` for what the stream said (the stats, the sample's
     attributes); the sample's colour is `frame.color`, BT.709 SDR limited range, chroma left
     (`AVCHROMA_LOC_LEFT`), as the stage's output is today;
   - when `frame.display_transform` is not `RAV1G_DISPLAY_TRANSFORM_NONE`: `CopyResource` of the image
     into a texture of `processedTexturePool` (the same texture size and format the stage's output has
     today) in place of `frameProcessor->Process`. The copy stays, because the player keeps samples and
     must never hold decoder images (the deadlock the comment there describes). A later step can let
     rav1g write the player's textures directly when the pool sizes allow it;
   - `ToneMappedOutputMark`, the media type at the display size and the fallback to `BaleToneMap` stay.
5. **The stats pane**: "Stage GPU" for an AV1 stream is rav1g's display pass, the `copy_plane` mark of
   `av1_perf_frame` (the GPU marks of the timing run); "Stage submit to GPU done" goes away for it.

## What Bale and the fork still do

- **The stage** for every other codec, for AV1 through FFmpeg/D3D11VA, for Hable and desaturation, and
  for a rav1g DLL of interface version 1.
- **Dither of NV12 on an 8-bit display** (`BALE_TONEMAP_OUTPUT_NV12_DITHERED`): rav1g rounds NV12. Bale
  uses P010 for AV1, so this matters only if an 8-bit output is ever chosen for it; then the fork keeps
  the stage, or rav1g gets the dither (an 8x8 ordered dither at the output pixel is a few instructions in
  the pass's quantization).
- **The colour attributes of the sample**, from `Rav1gFrame::color` (BT.709 SDR) and the HDR metadata
  (`mastering_display`, `content_light`, which stay the stream's for the stats).
- **The choice of the display size and the target**, which rav1g takes as given. A display mode change
  recreates the decoder (as a new sequence that needs `RAV1G_STATUS_NEEDS_RECONFIGURE` does).
- **One math in two places**: rav1g's pass is written from the standards in the order of Bale's stage
  and held to its own float32 model (`testing/display_pass/model.py`) at one code; holding both against
  Bale's CPU reference (`src/tonemap/cpu.rs`, 3 NV12 / 12 P010 codes) is a step for Bale's side.

## Savings

Per 4K60 frame on a 1080p display ([stage-fusion.md](stage-fusion.md#what-each-saves) for the bytes):

| | Today (rav1g 4K image + Bale's stage) | rav1g display pass |
| --- | --- | --- |
| Bytes moved for the output | ~156 MB (3 draws, 1 cross-API wait) | ~31 MB (1 dispatch) |
| Bale's stage on the console's shared GPU | up to ~5.6 ms SDR, ~9 ms HDR10 (Bale's logs) | none for AV1 |
| rav1g's output pass | fused into the last stage (~0.2 ms), or the 1.10 ms copy (frames without a postfilter) | the display pass: 1.98 ms measured for SDR before this branch; expected ~0.7-0.9 ms SDR, ~1.1-1.3 ms PQ (below) |
| Images | 4 x 23.7 MiB (4K P010) + 31.6 MiB intermediate | 4 x 5.9 MiB (1080p P010) |

Measured on the console before this branch (test player, idle state, SDR 10-bit 4K60, `-output p010
-displaypass 1920x1080`): the pass 1.98 ms a frame, rav1g's GPU busy 8.88 to 10.67 ms a frame at 40 Mbps
and 9.09 to 10.65 at 60 Mbps; Bale's SDR stage is up to ~5.6 ms, so the sum falls by ~3.8 ms even at
1.98 ms. Expected with this branch (not measured on the console; basis in
[stage-fusion.md](stage-fusion.md#the-display-pass-faster-reads-one-pipeline-a-mode)): SDR ~0.7-0.9 ms,
so rav1g's GPU a frame ~9.6 ms instead of 8.9 + Bale's 5.6, and HDR10 ~1.1-1.3 ms against Bale's ~9 ms,
which is what holds HDR10 at 60 Mbps at ~53 fps today.
