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
`frames_in_parse`, `frame_slots` and `threads` at their defaults. At 3840x2160 10-bit the decoder uses 487.8
MiB of device buffers, with two frames in parse, four slots and two threads. The GPU builds the
mode info grid by default.

## Loading the library at run time: the interface version

`rav1g_abi_version()` returns `RAV1G_ABI_VERSION` of the `rav1g.h` the library was built with. A loader
(`LoadPackagedLibrary`, `GetProcAddress`) calls it first and refuses a library whose value is not the
`RAV1G_ABI_VERSION` of the header it was compiled with. The number is raised whenever a declaration of
the header changes (a struct, an enum, a function), not for a constant, a comment or a change inside the
library: `rav1g-header --check` (CI) fails when the declarations of the header have changed and the
number has not (`tools/rav1g-header/abi.txt` has the fingerprint of the declarations of each version).
Version 1 is the interface since package 0.2.202; a library older than that has no `rav1g_abi_version`,
and a loader that finds no such export refuses it too. `rav1g_version()` is the text for a log.

## Releases

A tag `v<version>` (the workspace version reported by `rav1g_version()`) runs the release checks,
including the shader-pick and parser-switch conformance checks. The extended frame/thread matrix
and uavsync experiments have separate triggers. After the required checks pass, the `release`
job publishes a GitHub pre-release with the following files.

| File | What |
| --- | --- |
| `rav1g.dll` | the library of the default package (the C of clang, linked by lld-link, the app runtime, DXC `cs_6_0` shaders) |
| `rav1g.dll.lib`, `rav1g.pdb` | its import library and its symbols |
| `rav1g.h` | the interface it was built with (`RAV1G_ABI_VERSION`) |
| `rav1g-licenses.zip` | the licences a redistribution carries: rav1g (BSD-2-Clause), AOM's patent licence, rav1d, the notices of the linked crates |
| `SHA256SUMS` | the SHA-256 of each, for an application that pins the release |

An application copies `rav1g.h` from the release (or from the tagged commit) and pins `rav1g.dll` by
the tag and its SHA-256. The first release is `v0.4.0-alpha.1` (2026-11-18). `v0.4.0-alpha.2` (2026-11-18, interface version 1 as before) has the 96-byte mode info record, four pipeline slots and the mode info grid built by the GPU above 2560x1440 (487.8 MiB of device buffers at 3840x2160 10-bit), and the high-bit-depth intra shader with three group syncs. It is the last release of FXC `cs_5_0` shaders (branch `fxc-sm5`). `v0.5.0-alpha.1` (2026-11-19, interface version 1 as before) is the first of branch `main` with DXC `cs_6_0` shaders: on the console 2 to 4 % less GPU time a 4K frame than FXC (loop restoration, the output copy and inter prediction are faster), the same frames, and about 20 MiB more app memory for the pipeline states (116.6 instead of 96.0 MiB after they are created). A device without Shader Model 6.0 is refused: `rav1g_device_create` returns null and `rav1g_last_error` says why.

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
