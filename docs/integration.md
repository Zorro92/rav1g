# Integrating the decoder: seeking, the image pool, the device

Answers to the questions of an application that plays streams with the decoder through `rav1g.h`,
first asked for Bale's provider (2026-11-18). The contract is the header; this is how to use it.

## Seeking: drain, and go on with the same decoder

The decoder has no flush, and needs none: `rav1g_decoder_drain` waits for every frame in flight,
hands each to the output, calls `end_of_frames`, and leaves the decoder ready for the next temporal
unit. So a seek is:

1. Mark the frames that come out from now on as old (a generation number in `user_data`, which
   `Rav1gFrame::user_data` carries back), and stop sending samples.
2. `rav1g_decoder_drain`. The old frames arrive through `frame` and go straight back to the pool.
   With more than one frame in parse it returns the error of a frame of before the seek, one a call:
   call it until it returns `RAV1G_STATUS_OK`, and treat those errors as part of the seek, not as a
   failure of the stream.
3. Send the temporal units from the new place, starting with a key frame (a random access point). A
   key frame refreshes every reference, so nothing of before the seek is used.

This costs the frames in flight, at most one a pipeline slot (2 to 4; at 4K 3 slots, about 20 ms of
GPU each), and keeps the decoder's 0.5 GiB of device buffers where they are.

Destroying and creating a decoder works too, and is what a new sequence needs
(`RAV1G_STATUS_NEEDS_RECONFIGURE`: larger frames or more bits a sample than the decoder was created
for). On the console (package 0.2.198, 3840x2176 10-bit): `rav1g_decoder_create` about 0.11 s,
`rav1g_decoder_destroy` about 0.04 s, and the memory is back at once (the "MEMORY destroy" lines of
a decoder with a budget callback show the usage fall buffer kind by buffer kind). The pipeline states
are the device's and are not created again. So a recreate is about 0.15 s and a release and an
allocation of the device buffers; draining is cheaper and has no moment at which the memory is
counted twice.

A temporal unit after the seek that is not a key frame decodes with missing references: its frames
fail (`RAV1G_STATUS_CORRUPT_FRAME`, or the error of a frame whose references are missing) until the
next key frame. That is the seek landing in the wrong place, not a broken decoder: drop the frames,
keep the decoder.

## The image pool: `get_image` waits, it does not fail

`Rav1gOutput::get_image` either fills the image and returns 0, or returns another value, and that
**fails the temporal unit**: its frames are not decoded, and the frames that refer to them fail too,
until the next key frame. The decoder does not retry. What the header says instead: "The call may wait
until the application has an image free; the decoder then waits with it." So with a fixed pool,
`get_image` blocks until an image is back.

- **Where it is called.** On the thread that calls `rav1g_decoder_decode`, or, with more than one
  frame in parse (the default above 2560x1440), on a thread of the decoder. The pool has to be safe
  between that thread, the completion thread (`frame`, `unused`) and whoever returns images.
- **How many the decoder holds.** An image is the decoder's from `get_image` until `frame` (or
  `unused`) has returned; then it is the application's. The decoder holds at most one image a
  pipeline slot (`rav1g_decoder_configuration().frame_slots`, at most 4) and one for each of the two
  copies of a frame shown again (`show_existing_frame`) that can be in flight: at 4K with four slots,
  up to 6. The pool needs that many on top of what the application holds: the frames queued for the
  sample thread, those Media Foundation has (queued and on screen). If the pool is smaller, playback
  waits for the renderer to give one back, which it does as it plays.
- **A wait that has to end.** On a pause the renderer keeps its samples and gives nothing back, so the
  decoder may wait in `get_image` for as long as the pause lasts. A stop or a seek has to be able to
  end that wait: let `get_image` return a non-zero value when the stream is stopping or seeking. That
  temporal unit fails, which does not matter for a stream that is stopped (the decoder is destroyed),
  or for one that seeks (the next unit is a key frame). Treat that failure as the cancel it is.
- **One size.** The texture of an image has the width and the height of the request (rounded up to
  even). Film keeps one frame size, but a stream with frame size changes inside a sequence (resize
  mode, spatial layers) asks for other sizes. A pool of one size can return non-zero for a request of
  another size and fall back to another decoder, or keep a few textures of the other size.
- **The format.** `Rav1gImageRequest::format` is `RAV1G_IMAGE_FORMAT_NV12` for an 8-bit stream and
  `RAV1G_IMAGE_FORMAT_P010` for a 10-bit one, for every frame of the decoder: the pool's format comes
  from the stream's bit depth (`rav1g_peek_stream_info`).
- **The memory.** `Rav1gSettings::memory_available` has to hold back what the application will still
  allocate for its images. Create the pool before the decoder, or take its size off what the callback
  returns.

## One device for the process

A `Rav1gDevice` holds the D3D12 device, the queue and the pipeline states, and nothing of a stream.
Decoders made one after the other on it share nothing: each has its buffers, its threads and its
output fence. Nothing has to be reset between streams.

- **Create it early, once.** `rav1g_device_create` compiles the pipeline states (on threads, while the
  call returns); `rav1g_decoder_create` waits for them. Creating the device when the first AV1 stream
  is opened puts that wait in front of the first frame; at the start of the app it is gone by then.
- **Shader Model 6.0 is required.** `rav1g_device_create` asks the D3D12 device for
  `D3D12_FEATURE_SHADER_MODEL` before setting up the queue or compute pipelines. If the device
  reports a lower model or the query fails, the call returns null. Read `rav1g_last_error()` on the
  same thread for the reason and select another decoder. The Rust API reports `ErrorCode::Incapable`
  (`RAV1G_STATUS_INCAPABLE` in the C enum). The pointer-returning C call has no status return value.
  The default shaders require no wave intrinsics, native 16-bit operations, or optional typed UAV
  loads. This check applies to both application-supplied and decoder-created devices.
- **The decoder on `main` uses DXC `cs_6_0` throughout.** The shader-pick test build selects
  between default DXIL and DXC `-Od` per shader. The frozen FXC decoder and its player are on
  branch `fxc-sm5`. [Shader pick](test-player.md#shader-pick--shaderpick-the-dxc-package)
- **The output fence is the decoder's.** A new decoder has a new fence: open it on the D3D11 device
  again (`rav1g_decoder_output_fence`, `CreateSharedHandle`, `OpenSharedFence`), and close the handle
  of the old one.
- **The `rav1g_set_*` switches are for the whole process** (`rav1g_set_shader_pick`,
  `rav1g_set_log_callback`, `rav1g_set_perf_callback`, `rav1g_set_memory_stats` and the experiment
  switches). An application sets the log callback once and leaves the others alone.
- **A lost device ends everything on it.** After a device removed (the app over its memory limit, a
  driver reset) the device, every decoder on it and every shared texture have to be created again.
- **The adapter.** The D3D12 device has to be on the adapter of the D3D11 device that opens the
  shared textures and the fence. A console has one; on a desktop take the D3D11 device's
  (`IDXGIDevice::GetAdapter`), as the draft does.
- **Release.** `rav1g_device_destroy` releases the application's handle; the device lives on until
  its last decoder is destroyed.

## What the settings need

`rav1g_settings_default`, then the size and the bit depth of the sequence (from
`rav1g_peek_stream_info` on the first temporal unit with a sequence header: its largest frame size,
`max_frame_width` and `max_frame_height`, not the size of the first frame), the output callbacks,
`output_kind = RAV1G_OUTPUT_SEMI_PLANAR` and `shared_fence = true`. Leave `low_memory`,
`frames_in_parse`, `frame_slots` and `threads` at their defaults. At 3840x2160 10-bit the decoder uses 488.4
MiB of device buffers, with four frames in parse, four slots and four threads. The GPU builds the
mode info grid by default.

Four frames in parse on four threads is what makes high-bitrate 4K real time: these streams have one
tile, whose entropy decode is serial, so the frames in parse are what use the cores. On the console in
the idle state (test player, 4 threads), 2 against 4 frames in parse: a 100 Mbps 4K60 stream 53.4 against
63.6 fps, 46 Mbps 4K50 63.0 against 68.7, 40 Mbps 4K60 75.2 against 80.4; the device buffers are the same.
The parse threads are busy while there is parsing to do: on the 100 Mbps stream at 60 fps their CPU time
adds up to about 4.5 logical CPUs. An application that must leave more CPU free sets `threads` (and with
it at most as many frames in parse) lower.

## The display pass (interface version 2)

A draft on branch `exp/display-abi2-opus` (2026-11-23), with the 1:1 and 3:2 paths and the dither of
`exp/display-v2-opus`; not released. With `Rav1gSettings::display` the decoder writes every output image at a display size, downscaled and tone-mapped to BT.709 SDR, the
work an application's own stage does after the decoder today (Bale's D3D11 stage for 4K AV1 on a 1080p
display; the design, the costs and what it saves: [stage-fusion.md](stage-fusion.md), what Bale and its
fork would change: [bale-display-pass.md](bale-display-pass.md)). One compute pass at the end of the
frame's command list reads the decoded frame from its frame buffer and writes the image; the 4K output
image is not written at all, and frames without a postfilter stage lose their copy pass. The decoded
frames and the reference frames are the same bytes with and without it: the per-frame md5 and the Argon
results are taken from the frames, not from the display image.

**Zero is interface version 1's output.** `rav1g_settings_default` zeroes `display`; with `width` and
`height` 0 nothing changes. Every other field's 0 is its default too, so an application sets the size and
leaves the rest:

```c
Rav1gSettings settings;
rav1g_settings_default(&settings);
/* ... size, bit depth, output callbacks ... */
settings.output_kind = RAV1G_OUTPUT_SEMI_PLANAR;   /* needed */
settings.display.width = 1920;                     /* the image size */
settings.display.height = 1080;
/* optional: settings.display.source_peak_nits = 1000; target_white_nits = 100; depth = 10;
   dither = RAV1G_DISPLAY_DITHER_AUTO;
   assumed_* = the container's code points where the sequence header says "unspecified";
   max_cll_nits, mastering_max_nits = the container's HDR metadata (version 3) */
```

| Field | Meaning | 0 |
| --- | --- | --- |
| `width`, `height` | the size of every image, rounded up to even, at most 16384; any ratio to the frame (exactly half, the same size and two thirds in both are the fast paths: 4K to 1080p, 4K to 4K, 4K to 1440p) | no display pass |
| `transform` | `RAV1G_DISPLAY_TRANSFORM_AUTO`, or one of the transforms below for every frame whatever its colour says | by each frame's colour |
| `tone_curve` | `RAV1G_TONE_CURVE_BT2390`; `RAV1G_TONE_CURVE_HABLE` is not in this version (`RAV1G_STATUS_INCAPABLE`) | BT.2390 |
| `source_peak_nits` | the light the EETF maps to the SDR range for HLG, and for PQ without HDR metadata ([the source peak](#the-source-peak-and-the-input-chroma-interface-version-3)), up to 10000 | 1000 |
| `target_white_nits` | the SDR white; the EETF ends at 2.03 times it (203 nits for 100); up to 4926 (Bale's fork passes 80 to 400) | 100 |
| `desaturation` | not in this version: 0 (another value is `RAV1G_STATUS_INCAPABLE`) | none |
| `depth` | 10: P010; 8: NV12 | by `Rav1gSettings::bit_depth` |
| `dither` | `RAV1G_DISPLAY_DITHER_AUTO` (an ordered dither for NV12, rounded P010), `_OFF` (rounded), `_ORDERED` (dithered at either depth) | auto |
| `assumed_primaries`, `assumed_transfer_characteristics`, `assumed_matrix_coefficients` | H.273 code points taken where the sequence header says unspecified (2) | none: unspecified is BT.709 SDR |
| `assumed_chroma_sample_position` | the AV1 `chroma_sample_position` taken where the header says 0 (unknown) | none: unknown is between rows |
| `max_cll_nits`, `mastering_max_nits` | version 3: the application's HDR metadata (its container's MaxCLL and mastering display maximum), nits; each fills what the stream's metadata OBUs do not say | none |
| `reserved` (1; 3 in version 2) | zero | |

**The transforms** (`RAV1G_DISPLAY_TRANSFORM_*`; the same numbers are `Rav1gFrame::display_transform`):

| Transform | Frames (by their transfer and primaries) | What the pass does |
| --- | --- | --- |
| `SCALE` (1) | SDR with BT.709 primaries (or any but BT.2020) | BT.1886 decode (power 2.4), area average, encode; no clip before the encode |
| `GAMUT` (2) | SDR with BT.2020 primaries | as `SCALE`, BT.2020 to BT.709 (BT.2087), one clip |
| `PQ` (3) | PQ (transfer 16) | ST 2084 EOTF, the BT.2390 EETF on the largest channel from the source peak to 2.03 x white, BT.2020 to BT.709 when the primaries are BT.2020, one clip |
| `HLG` (4) | HLG (transfer 18) | inverse OETF and the OOTF of a 1000-nit display (BT.2100), then as `PQ` |

In every case: the chroma at each source pixel by a 4x4 Catmull-Rom cubic at its siting
(`chroma_sample_position`: colocated, or between rows for vertical and unknown; version 2: bilinear),
clamped to the code range, the matrix of the
stream (BT.2020, BT.601, else BT.709 weights) and its range; each output pixel the average of the source
pixels its footprint covers, weighted by their overlap, in display-linear light; the output chroma mixed
in linear light over three output columns and two rows (left-sited); BT.709 limited-range codes. Film
grain (AV1 7.18.3) is applied to the frame before it is downscaled. A monochrome frame takes the pass
too, with neutral chroma, so that every image of a decoder has one size.

**The dither.** The last quantisation is `floor(v + t)`: `t` is 1/2 (rounding), or with the dither a
threshold at the output sample from the R2 sequence (M. Roberts, "The unreasonable effectiveness of
quasirandom sequences", 2018): `frac(x / g + y / g^2)` for the plastic number g = 1.3247..., in 32-bit
fixed point (`x * 0xc13fa9a9 + y * 0x91e10da6`), its top 8 bits, `(t8 + 1/2) / 256`. Luma takes it at its
pixel, Cb at its chroma sample, Cr at the transposed sample (x and y swapped), so that Cb and Cr do not
step together. Why this one: its 256 levels have mean 1/2, so a flat area keeps its level and every code
is within one of the exact value; it has no period (an 8x8 Bayer matrix's cross-hatch is a period of 8)
and its rows and columns are unbiased: on a slow 8-bit ramp the column means stay within 0.01 of a code of
the exact value, where Bayer's are up to 0.32 off; filtered like the eye at a few pixels (a Gaussian of
1.5-2.5 pixels) its noise is the same as Bayer's (0.018 against 0.015 of a code rms); it needs no table
(integer arithmetic, about 6 instructions a sample); and it is the same for every frame, so a still picture
stays still. `testing/display_pass` holds it bit for bit against the model, and fails a run in which more
than 2% of the codes of a plane differ (a dither missing or not the model's moves about half of them).

**The images.** `Rav1gImageRequest::width` and `height` are the display size, `format` is
`RAV1G_IMAGE_FORMAT_P010` for depth 10 and `RAV1G_IMAGE_FORMAT_NV12` for depth 8, for every frame.
The image has to be the texture alone, created with `D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS`
(`D3D11_BIND_UNORDERED_ACCESS` for a shared D3D11 texture): the pass writes it, no copy does. Another
image (a buffer, a texture shaders may not write) fails the temporal unit with a "DISPLAY" log line,
as a texture of the wrong size or format does. A device that lets no shader write the format gets no
decoder: `rav1g_decoder_create` returns `RAV1G_STATUS_INCAPABLE` ("DISPLAY ..." in
`rav1g_last_error`), and the application keeps its own stage.

**The frames.** With a display pass, `Rav1gFrame::display_transform` says which transform the frame
took; `width`, `height`, `bit_depth` and `format` describe the image; `monochrome` is false; `color` is
the image's BT.709 SDR (primaries, transfer and matrix 1, limited range, chroma sample position 1). What
was decoded is in `source_width`, `source_height` (after super-resolution) and `source_color` (the
sequence header's code points as the stream has them, not the assumed ones). The HDR metadata stays the
stream's. Without a display pass `display_transform` is `RAV1G_DISPLAY_TRANSFORM_NONE` (0) and the
`source_*` fields repeat `width`, `height` and `color`. `display_source_peak_nits` (version 3) is the
source peak the frame's tone map took ([below](#the-source-peak-and-the-input-chroma-interface-version-3)),
0 without a tone map (SDR, no display pass).

**A sequence change** keeps the display size and depth of the decoder: the images keep their size and
format. A new sequence with another colour description (SDR to PQ, say) changes the transform from its
first frame on; one with another frame size changes `source_width` and `source_height`; one that needs
`RAV1G_STATUS_NEEDS_RECONFIGURE` is a new decoder, created with the same `display`. A display mode
change (another size or depth) is a new decoder too. A frame shown again (`show_existing_frame`) takes the
pass again into its own image.

**The cost** is on the decoder's queue: one dependent step and the pass a shown frame, bound by its
arithmetic (the console's slice of the GPU, 1.86 TFLOP/s), not by its bytes. Per 4K 10-bit frame, P010
(measured on the console for 2:1, idle state; the rest expected from the RDNA2 listing scaled by what
2:1 measured, basis in [stage-fusion.md](stage-fusion.md#11-32-and-the-dither)):

| Image | Path | Bytes (read + written) | SDR | PQ (HDR10) | HLG |
| --- | --- | ---: | ---: | ---: | ---: |
| 1920x1080 | 2:1 | 24.9 + 6.2 MB | 1.01 ms measured | 1.19 measured, ~1.16 now | ~1.45 |
| 2560x1440 | 3:2 | 24.9 + 11.1 MB | ~1.6 | ~1.8 | ~2.0 |
| 3840x2160 | 1:1 | 24.9 + 24.9 MB | ~2.3 | ~2.45 | ~2.7 |
| other | area loop | | slower (36 reads a quad, one after the other) | | |

NV12 with the dither adds ~0.03 ms at 1080p and ~0.1 ms at 4K and writes a quarter less. The timing
run's stage `display` (`AV1_PERF_GPU_DISPLAY` of `av1_perf_frame`, with `output_path`
`AV1_PERF_OUTPUT_DISPLAY`) is the pass; `copy_plane` is not dispatched.

**Exactness.** The image is not bit-exact against an application's own stage, nor across GPUs: float32
in a compute shader. `testing/display_pass` holds every code against a float32 model of the pass
(`model.py`) within one code: the shader compiled as C++ on test frames (precheck, CI `sync-check`) and
the decoder on WARP (CI `rust-output`). Against Bale's own CPU reference of its stage (its default
settings, P010, on the C++ check's frames: 2:1, 1:1, 3:2 and the area loop; PQ, HLG, BT.2020 and BT.709
SDR; both sitings, full range) the pass is within one code as well, at most 0.5% of a plane's codes one
apart ([bale-display-pass.md](bale-display-pass.md#how-close-to-bales-stage)).

**Not in this version**: the Hable curve, desaturation, HDR (PQ) targets, the AV1 render size (the pass
takes the upscaled frame), other primaries than BT.709 and BT.2020 (taken as BT.709), and the BT.2020
constant-luminance matrix (taken as NCL).

### The source peak and the input chroma (interface version 3)

Two changes that follow Bale's D3D11 stage (its docs/gpu-tone-map.md, "Source peak from the stream's HDR
metadata" and "Cubic input chroma", 2026-11-24), so that the two look the same.

**The source peak.** A PQ (ST 2084) frame is tone-mapped (the BT.2390 EETF) from the peak of its HDR
metadata, not from a fixed one:

1. MaxCLL, when it is above 0; else
2. the mastering display's largest luminance, when it is above 0; else
3. `source_peak_nits` (0 is 1000), as configured.

When both are there and MaxCLL is above the mastering display's (no pixel is brighter than the display it
was graded on: broken metadata), the mastering display's is taken. A peak from the metadata is clamped to
400..10000 nits (lower is broken metadata, 10000 is the end of PQ); the configured one is taken as it is.
HLG keeps the configured peak (its OOTF renders for a 1000-nit display already), SDR has no tone map.
Bale's checks of its stage: MaxCLL 4057 with a mastering display of 1000 gives 1000; MaxCLL 1184 with 4000
gives 1184; no metadata gives the configured 1000.

Each value is the stream's: its metadata OBUs `METADATA_TYPE_HDR_CLL` (`max_cll`, cd/m2) and
`METADATA_TYPE_HDR_MDCV` (`luminance_max`, 24.8 fixed point) (AV1 5.8.3, 5.8.4, 6.7.3, 6.7.4), the ones that
hold for the frame by rav1d's rule (from the OBU on until one of its kind replaces it; the same values as
`Rav1gFrame::content_light` and `mastering_display`). Where the stream says nothing, or 0, the
application's `max_cll_nits` and `mastering_max_nits` (for example Matroska's MaxCLL and mastering
metadata) fill it: the stream wins, as for the assumed colour code points, value by value.
`Rav1gFrame::display_source_peak_nits` says what each frame took, for a log. The peak changes only with
the metadata (in practice once a stream); the tables of the tone map are made again then on the CPU (well
under a millisecond).

**The input chroma.** Where the pass needs the chroma at a source pixel, it takes the separable 4x4
Catmull-Rom cubic (B 0, C 1/2; Keys' a = -1/2) of the coded chroma samples at the pixel's place, at the
stream's siting (`chroma_sample_position`, AV1 6.4.2, with the assumed one; unknown is between rows),
before any transfer or matrix, with the sample coordinates clamped to the plane, and the result clamped to
the code range (limited range 16..240 at 8 bits, 64..960 at 10; full range every code). The rest of the
pass is as before: the area downscale in linear light, the output chroma averaged in linear light, one
clip at the end. A luma pixel sits at a quarter phase of the chroma grid (0, 1/4, 1/2, 3/4), where every
weight is a multiple of 1/128: with integer codes every product and sum is exact in float32, so the pass
and its model have the same chroma bit for bit. The cubic passes through the samples and reproduces a line
and a parabola, so fine colour detail and colour edges stay sharper than with bilinear; its negative
lobes overshoot a sharp edge by about 7% of the step a side, which the clamp keeps in the code range.

Cost (expected; to be measured on the console; [stage-fusion.md](stage-fusion.md#catmull-rom-chroma-the-source-peak-from-the-metadata-interface-version-3)):
one source row a step on every fast path, its luma and its four tap rows of U and V (five columns each)
issued together before the arithmetic, no group memory and no more syncs; the five columns filtered down
with constant weights, then across. About 23 instructions a source pixel more than bilinear: at the
console's 1.86 TFLOP/s ~+0.2 ms a 4K frame at 2:1, ~+0.25-0.3 ms at 1:1 and 3:2, ~+0.3-0.5 ms for the
area loop (1080p to 4K); Bale measured its own cubic chroma at ~1.1 ms a 4K frame. The SDR and HLG
pipelines go from 8 and 6 waves a SIMD to 4 (96 VGPRs; PQ stays at 4 with 110): the first thing to check
on the console. Sized for the GPU slice: no new memory, nothing on the CPU.

**A/B switches** (internal, `libav1/dx/av1_perf.h`, not in `rav1g.h`): `av1_set_display_fixed_peak(1)`
takes `source_peak_nits` for every PQ frame whatever its metadata, `av1_set_display_bilinear_chroma(1)`
the bilinear chroma of version 2 (test flags `-displayfixedpeak`, `-displaybilinearchroma`; Bale's
`tonemap-debug fixedpeak`, `bilinearchroma`). Off by default.

## Interface version 3: the retired switches, the source peak and the input chroma

A draft, not released: branch `exp/cleanup-opus` (fingerprint `267a0692fc675929`) took out the retired
switches; branch `exp/display-parity-opus` (fingerprint `f4f314d6ea9678c2`) adds to the same version the
display pass's source peak from the HDR metadata and the Catmull-Rom input chroma
([above](#the-source-peak-and-the-input-chroma-interface-version-3)): `Rav1gDisplay::max_cll_nits` and
`mastering_max_nits` in two of its three reserved words (the struct keeps its size; `reserved` is one
word), `Rav1gFrame::display_source_peak_nits` behind `source_color`. A display pass of version 2 with
these at 0 differs only in its image: PQ streams with metadata take its peak, every stream the cubic
chroma (the two `av1_*` switches give version 2's image back).

The retired switches: `rav1g.h` loses what only test runs used, A/B switches whose new path is the default
and experiments that lost ([test-player.md](test-player.md#retired-switches)):

- the functions `rav1g_set_cdef_v2`, `rav1g_set_intra_opt`, `rav1g_set_clears`, `rav1g_set_async_clear`,
  `rav1g_set_lf_h_split` and `rav1g_set_intra_edge_fix`, and the constants `RAV1G_CLEARS_*`. Their
  defaults are what the decoder does: CDEF through `cdef_filter_v3`, the optimized intra kernels, no
  clears of a slot (the wavefront of intra prediction at the edges of the tiles), the horizontal loop
  filter in one dispatch per plane.
- the parser option bits `RAV1G_PARSER_OPTION_LATE_LISTS` (16), `_CLEAR_GRID` (32), `_GPU_GRID_CHECK`
  (1024) and `_GPU_PRED_LISTS` (4096). The other bits keep their values, and
  `RAV1G_PARSER_OPTIONS_DEFAULT` is 2566 as before. `rav1g_decoder_create` refuses settings with one of
  the four retired bits (`RAV1G_STATUS_INVALID_PARAM`, the bits in `rav1g_last_error`) rather than
  ignoring them; 1 and 256, retired earlier, still mean nothing.

An application that uses none of them (`rav1g_settings_default`, the perf callback) only takes the new
header and checks for version 3. `av1_perf.h`, which an application may copy for the timing record,
loses the declarations of the internal `av1_*` setters of the same switches; `av1_perf_frame` keeps its
layout (640 bytes on x64, 22 GPU stages: `slot_clear`, `pred_count` and `pred_scan` keep their places and
have no time any more).

## Loading the library at run time: the interface version

`rav1g_abi_version()` returns `RAV1G_ABI_VERSION` of the `rav1g.h` the library was built with. A loader
(`LoadPackagedLibrary`, `GetProcAddress`) calls it first and refuses a library whose value is not the
`RAV1G_ABI_VERSION` of the header it was compiled with. The number is raised whenever a declaration of
the header changes (a struct, an enum, a function), not for a constant, a comment or a change inside the
library: `rav1g-header --check` (CI) fails when the declarations of the header have changed and the
number has not (`tools/rav1g-header/abi.txt` has the fingerprint of the declarations of each version).
Version 1 is the interface since package 0.2.202; a library older than that has no `rav1g_abi_version`,
and a loader that finds no such export refuses it too. `rav1g_version()` is the text for a log.
Version 2 (a draft, not released: branch `exp/display-abi2-opus` had fingerprint `d754ced4ce571264`,
`exp/display-v2-opus` has `4c4ff3bb6491dd86`, with `Rav1gDisplay::dither` in the first of its four
reserved words) adds the display pass: `Rav1gDisplay`, `Rav1gSettings::display` behind `output`,
`Rav1gFrame::display_transform`, `source_width`, `source_height` and `source_color` behind `format`, the
`RAV1G_DISPLAY_TRANSFORM_*`, `RAV1G_TONE_CURVE_*` and `RAV1G_DISPLAY_DITHER_*` constants ([The display pass](#the-display-pass-interface-version-2)). An application
of version 1 recompiled against it and filling the settings with `rav1g_settings_default` gets the
output of version 1. Version 3 (a draft, not released: branch `exp/display-parity-opus`, fingerprint `f4f314d6ea9678c2`;
`exp/cleanup-opus` had `267a0692fc675929`) takes the retired switches out and adds the display pass's
source peak and input chroma ([Interface version 3](#interface-version-3-the-retired-switches-the-source-peak-and-the-input-chroma)).

## Releases

A tag `v<version>` (the workspace version reported by `rav1g_version()`) runs the release checks,
including the shader-pick and parser-switch conformance checks. The extended frame/thread matrix
and uavsync experiments have separate triggers. After the required checks pass, the `release`
job publishes a GitHub pre-release with the following files.

| File | What |
| --- | --- |
| `rav1g.dll` | the library of the default package (no C, linked by lld-link, the app runtime, DXC `cs_6_0` shaders) |
| `rav1g.dll.lib`, `rav1g.pdb` | its import library and its symbols |
| `rav1g.h` | the interface it was built with (`RAV1G_ABI_VERSION`) |
| `rav1g-licenses.zip` | the licences a redistribution carries: rav1g (BSD-2-Clause), AOM's patent licence, rav1d, the notices of the linked crates |
| `SHA256SUMS` | the SHA-256 of each, for an application that pins the release |

An application copies `rav1g.h` from the release (or from the tagged commit) and pins `rav1g.dll` by
the tag and its SHA-256. The first release is `v0.4.0-alpha.1` (2026-11-18). `v0.4.0-alpha.2` (2026-11-18, interface version 1 as before) has the 96-byte mode info record, four pipeline slots and the mode info grid built by the GPU above 2560x1440 (487.8 MiB of device buffers at 3840x2160 10-bit), and the high-bit-depth intra shader with three group syncs. It is the last release of FXC `cs_5_0` shaders (branch `fxc-sm5`). `v0.5.0-alpha.1` (2026-11-19, interface version 1 as before) is the first of branch `main` with DXC `cs_6_0` shaders: on the console 2 to 4 % less GPU time a 4K frame than FXC (loop restoration, the output copy and inter prediction are faster), the same frames, and about 20 MiB more app memory for the pipeline states (116.6 instead of 96.0 MiB after they are created). A device without Shader Model 6.0 is refused: `rav1g_device_create` returns null and `rav1g_last_error` says why. `v0.5.0-alpha.2` (2026-11-20, interface version 1 as before) has every shader ported to Shader Model 6 (intra prediction, loop restoration, loop filter, film grain, inter prediction, CDEF, the copy, transform and clear passes): on the console the heaviest second of a 4K 10-bit film with film grain takes about 790 instead of about 980 ms of GPU time (24.4 to 30.5 frames a second at full speed), with the same frames and the same device memory (488.8 MiB at 3840x2176 10-bit with four slots). It decodes all 765 valid streams of the Argon Profile 0 conformance set within its 4096x2176 limit bit-exact ([testing/argon/RESULTS.md](../testing/argon/RESULTS.md)): the fixes are in palette blocks, 10-bit intra block copy, overlapped prediction, film grain of odd sizes and buffers of dense or narrow frames, and the mode info pools now grow when a frame needs more. The library has no C any more (its shader table is Rust). `v0.6.0-alpha.1` (2026-11-22, interface version 1 as before) plays high-bitrate 4K in real time on the console in the idle state: a 40 Mbps 10-bit 4K60 stream with no dropped frame, a 46 Mbps 10-bit 4K50 stream with 4 late frames in 300 (key frames), where `v0.5.0-alpha.2` dropped hundreds. The parse gives the frames of one-tile streams to more cores: four threads and four frames in parse are the default above 2560x1440 (two and two before; a 100 Mbps 4K60 stream 50.8 to 63.9 frames a second at full speed, 46 Mbps 4K50 62.9 to 68.2, 40 Mbps 4K60 72.1 to 80.7), the block lists are counted a superblock row behind the entropy decoder on idle parse threads, a frame waits only for the reference rows its entropy decode reads, AC tokens are packed and the film grain templates are built on the CPU. The GPU side has faster intra wavefront steps (the heaviest second of the 4K 10-bit film with grain 790 to about 680 ms of GPU time) and a fix to the count of the samples above a block that the intra edge filter reads (AV1 7.11.2.4) in the 10-bit and second intra shaders. The device buffers are the same (488.4 MiB at 3840x2176 10-bit with four slots); the parse threads may keep up to about 4.5 logical CPUs busy on the heaviest streams (`threads` caps them). `v0.6.0-alpha.2` (2026-11-23, interface version 1 as before) takes less GPU time per 4K frame when the output is a semi-planar texture the decoder's shaders may write (`RAV1G_OUTPUT_SEMI_PLANAR` with unordered access, what Bale's image pool creates): the last postfilter stage (CDEF, super-resolution or loop restoration) writes the NV12/P010 output image itself instead of a separate full-frame copy pass, and a frame no later frame can read (refresh_frame_flags 0, AV1 7.20) skips its frame buffer write. On the console a 40 Mbps 10-bit 4K60 stream takes 8.9 instead of 9.9 ms of GPU time a frame, a 100 Mbps one 9.3 instead of 9.8; frames without any postfilter still take the copy. CDEF uses packed skip masks (64 KiB more device memory over four slots, 488.5 MiB at 3840x2176 10-bit). Same frames. `v0.6.0-alpha.3` (2026-11-23, interface version 1 as before) is the same decoder; its timing record (`av1_perf_frame`, the internal `av1_perf.h` that an application copies) ends with the user data of the frame's temporal unit, so that an application can join rav1g's per-frame times with its own log; the record grows from 624 to 632 bytes (a copy of the header updates its size check). `v0.7.0-alpha.1` (2026-11-24) is interface version 2: the optional display pass (`Rav1gSettings::display`, "The display pass" above) writes each output image at the display size, downscaled in linear light and, for HDR on an SDR display, tone-mapped (BT.2390) and gamut-converted to BT.709, in one pass at the end of the frame; with `display` all zero (`rav1g_settings_default`) the output is that of version 1. On the console the pass takes 1.0 / 1.2 ms per 4K frame (SDR / HDR10) at 1920x1080, 1.1 / 1.5 ms at 2560x1440 and 1.5 / 1.8 ms at 3840x2160 (1:1 tone map), against 1.1 ms for the plain 4K output it replaces. It also has the CPU parse changes since `v0.6.0-alpha.3` (1-2.5% faster on the console), and the timing record grows to 640 bytes (`AV1_PERF_GPU_DISPLAY`, `user_data` at 632).

## A pipeline cache on disk: tried, and taken out (2026-11-18)

Compiling the 99 pipeline states takes about 6.5 s and 77 MiB of app memory on the console at every
device creation. A cache on disk (`rav1g_device_create_with_cache`, ABI 2, never released) kept them in
the app's LocalCache folder, as a D3D12 pipeline library or as the cached blob of each pipeline state;
on WARP both worked. On the console both removed the D3D12 device in a UWP app
(`CreatePipelineLibrary` and `GetCachedBlob` fail with `DXGI_ERROR_DEVICE_REMOVED`,
`GetDeviceRemovedReason` `DXGI_ERROR_DRIVER_INTERNAL_ERROR`), although the device reports both kinds of
cache as supported, and the GPU ran about 2.6 times slower afterwards until the console was restarted.
So the cache is gone again (the commits d9ad86a, d023fda, c9c93ab and 2030cac reverted) and the
interface is version 1 as released. An application hides the 6.5 s by creating the device early, before
the first stream needs it (Bale does it when an AV1 title's Detail page opens).
