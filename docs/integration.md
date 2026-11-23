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

A draft on branch `exp/display-abi2-opus` (2026-11-23), not released. With `Rav1gSettings::display`
the decoder writes every output image at a display size, downscaled and tone-mapped to BT.709 SDR, the
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
   assumed_* = the container's code points where the sequence header says "unspecified" */
```

| Field | Meaning | 0 |
| --- | --- | --- |
| `width`, `height` | the size of every image, rounded up to even, at most 16384; any ratio to the frame (exactly half in both is the fast path) | no display pass |
| `transform` | `RAV1G_DISPLAY_TRANSFORM_AUTO`, or one of the transforms below for every frame whatever its colour says | by each frame's colour |
| `tone_curve` | `RAV1G_TONE_CURVE_BT2390`; `RAV1G_TONE_CURVE_HABLE` is not in this version (`RAV1G_STATUS_INCAPABLE`) | BT.2390 |
| `source_peak_nits` | the PQ and HLG light the EETF maps to the SDR range, up to 10000 | 1000 |
| `target_white_nits` | the SDR white; the EETF ends at 2.03 times it (203 nits for 100) | 100 |
| `desaturation` | not in this version: 0 (another value is `RAV1G_STATUS_INCAPABLE`) | none |
| `depth` | 10: P010; 8: NV12 (rounded, not dithered) | by `Rav1gSettings::bit_depth` |
| `assumed_primaries`, `assumed_transfer_characteristics`, `assumed_matrix_coefficients` | H.273 code points taken where the sequence header says unspecified (2) | none: unspecified is BT.709 SDR |
| `assumed_chroma_sample_position` | the AV1 `chroma_sample_position` taken where the header says 0 (unknown) | none: unknown is between rows |
| `reserved` | zero | |

**The transforms** (`RAV1G_DISPLAY_TRANSFORM_*`; the same numbers are `Rav1gFrame::display_transform`):

| Transform | Frames (by their transfer and primaries) | What the pass does |
| --- | --- | --- |
| `SCALE` (1) | SDR with BT.709 primaries (or any but BT.2020) | BT.1886 decode (power 2.4), area average, encode; no clip before the encode |
| `GAMUT` (2) | SDR with BT.2020 primaries | as `SCALE`, BT.2020 to BT.709 (BT.2087), one clip |
| `PQ` (3) | PQ (transfer 16) | ST 2084 EOTF, the BT.2390 EETF on the largest channel from the source peak to 2.03 x white, BT.2020 to BT.709 when the primaries are BT.2020, one clip |
| `HLG` (4) | HLG (transfer 18) | inverse OETF and the OOTF of a 1000-nit display (BT.2100), then as `PQ` |

In every case: the chroma at each source pixel by bilinear interpolation at its siting
(`chroma_sample_position`: colocated, or between rows for vertical and unknown), the matrix of the
stream (BT.2020, BT.601, else BT.709 weights) and its range; each output pixel the average of the source
pixels its footprint covers, weighted by their overlap, in display-linear light; the output chroma mixed
in linear light over three output columns and two rows (left-sited); BT.709 limited-range codes. Film
grain (AV1 7.18.3) is applied to the frame before it is downscaled. A monochrome frame takes the pass
too, with neutral chroma, so that every image of a decoder has one size.

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
`source_*` fields repeat `width`, `height` and `color`.

**A sequence change** keeps the display size and depth of the decoder: the images keep their size and
format. A new sequence with another colour description (SDR to PQ, say) changes the transform from its
first frame on; one with another frame size changes `source_width` and `source_height`; one that needs
`RAV1G_STATUS_NEEDS_RECONFIGURE` is a new decoder, created with the same `display`. A display mode
change (another size or depth) is a new decoder too. A frame shown again (`show_existing_frame`) takes the
pass again into its own image.

**The cost** is on the decoder's queue: one dependent step and the pass a shown frame, 31 MB moved at
3840x2160 10-bit to 1920x1080 (the frame buffer read, the image written) against ~156 MB of the 4K image
and a three-draw stage. On the console before this branch the pass took 1.98 ms a 4K SDR frame; this
branch expects ~0.7-0.9 ms for SDR and ~1.1-1.3 ms for PQ (not measured yet; the basis is in
[stage-fusion.md](stage-fusion.md#the-display-pass-faster-reads-one-pipeline-a-mode)). The timing run's
`copy_plane` mark is the pass.

**Exactness.** The image is not bit-exact against an application's own stage, nor across GPUs: float32
in a compute shader. `testing/display_pass` holds every code against a float32 model of the pass
(`model.py`) within one code: the shader compiled as C++ on test frames (precheck, CI `sync-check`) and
the decoder on WARP (CI `rust-output`).

**Not in this version**: the Hable curve, desaturation, the NV12 dither, HDR (PQ) targets, the AV1
render size (the pass takes the upscaled frame), other primaries than BT.709 and BT.2020 (taken as
BT.709), and the BT.2020 constant-luminance matrix (taken as NCL).

## Loading the library at run time: the interface version

`rav1g_abi_version()` returns `RAV1G_ABI_VERSION` of the `rav1g.h` the library was built with. A loader
(`LoadPackagedLibrary`, `GetProcAddress`) calls it first and refuses a library whose value is not the
`RAV1G_ABI_VERSION` of the header it was compiled with. The number is raised whenever a declaration of
the header changes (a struct, an enum, a function), not for a constant, a comment or a change inside the
library: `rav1g-header --check` (CI) fails when the declarations of the header have changed and the
number has not (`tools/rav1g-header/abi.txt` has the fingerprint of the declarations of each version).
Version 1 is the interface since package 0.2.202; a library older than that has no `rav1g_abi_version`,
and a loader that finds no such export refuses it too. `rav1g_version()` is the text for a log.
Version 2 (the draft of branch `exp/display-abi2-opus`, fingerprint `d754ced4ce571264`) adds the display
pass: `Rav1gDisplay`, `Rav1gSettings::display` behind `output`, `Rav1gFrame::display_transform`,
`source_width`, `source_height` and `source_color` behind `format`, the `RAV1G_DISPLAY_TRANSFORM_*` and
`RAV1G_TONE_CURVE_*` constants ([The display pass](#the-display-pass-interface-version-2)). An application
of version 1 recompiled against it and filling the settings with `rav1g_settings_default` gets the
output of version 1.

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
the tag and its SHA-256. The first release is `v0.4.0-alpha.1` (2026-11-18). `v0.4.0-alpha.2` (2026-11-18, interface version 1 as before) has the 96-byte mode info record, four pipeline slots and the mode info grid built by the GPU above 2560x1440 (487.8 MiB of device buffers at 3840x2160 10-bit), and the high-bit-depth intra shader with three group syncs. It is the last release of FXC `cs_5_0` shaders (branch `fxc-sm5`). `v0.5.0-alpha.1` (2026-11-19, interface version 1 as before) is the first of branch `main` with DXC `cs_6_0` shaders: on the console 2 to 4 % less GPU time a 4K frame than FXC (loop restoration, the output copy and inter prediction are faster), the same frames, and about 20 MiB more app memory for the pipeline states (116.6 instead of 96.0 MiB after they are created). A device without Shader Model 6.0 is refused: `rav1g_device_create` returns null and `rav1g_last_error` says why. `v0.5.0-alpha.2` (2026-11-20, interface version 1 as before) has every shader ported to Shader Model 6 (intra prediction, loop restoration, loop filter, film grain, inter prediction, CDEF, the copy, transform and clear passes): on the console the heaviest second of a 4K 10-bit film with film grain takes about 790 instead of about 980 ms of GPU time (24.4 to 30.5 frames a second at full speed), with the same frames and the same device memory (488.8 MiB at 3840x2176 10-bit with four slots). It decodes all 765 valid streams of the Argon Profile 0 conformance set within its 4096x2176 limit bit-exact ([testing/argon/RESULTS.md](../testing/argon/RESULTS.md)): the fixes are in palette blocks, 10-bit intra block copy, overlapped prediction, film grain of odd sizes and buffers of dense or narrow frames, and the mode info pools now grow when a frame needs more. The library has no C any more (its shader table is Rust). `v0.6.0-alpha.1` (2026-11-22, interface version 1 as before) plays high-bitrate 4K in real time on the console in the idle state: a 40 Mbps 10-bit 4K60 stream with no dropped frame, a 46 Mbps 10-bit 4K50 stream with 4 late frames in 300 (key frames), where `v0.5.0-alpha.2` dropped hundreds. The parse gives the frames of one-tile streams to more cores: four threads and four frames in parse are the default above 2560x1440 (two and two before; a 100 Mbps 4K60 stream 50.8 to 63.9 frames a second at full speed, 46 Mbps 4K50 62.9 to 68.2, 40 Mbps 4K60 72.1 to 80.7), the block lists are counted a superblock row behind the entropy decoder on idle parse threads, a frame waits only for the reference rows its entropy decode reads, AC tokens are packed and the film grain templates are built on the CPU. The GPU side has faster intra wavefront steps (the heaviest second of the 4K 10-bit film with grain 790 to about 680 ms of GPU time) and a fix to the count of the samples above a block that the intra edge filter reads (AV1 7.11.2.4) in the 10-bit and second intra shaders. The device buffers are the same (488.4 MiB at 3840x2176 10-bit with four slots); the parse threads may keep up to about 4.5 logical CPUs busy on the heaviest streams (`threads` caps them). `v0.6.0-alpha.2` (2026-11-23, interface version 1 as before) takes less GPU time per 4K frame when the output is a semi-planar texture the decoder's shaders may write (`RAV1G_OUTPUT_SEMI_PLANAR` with unordered access, what Bale's image pool creates): the last postfilter stage (CDEF, super-resolution or loop restoration) writes the NV12/P010 output image itself instead of a separate full-frame copy pass, and a frame no later frame can read (refresh_frame_flags 0, AV1 7.20) skips its frame buffer write. On the console a 40 Mbps 10-bit 4K60 stream takes 8.9 instead of 9.9 ms of GPU time a frame, a 100 Mbps one 9.3 instead of 9.8; frames without any postfilter still take the copy. CDEF uses packed skip masks (64 KiB more device memory over four slots, 488.5 MiB at 3840x2176 10-bit). Same frames.

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
