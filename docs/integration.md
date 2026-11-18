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
  copies of a frame shown again (`show_existing_frame`) that can be in flight: at 4K with three slots,
  up to 5. The pool needs that many on top of what the application holds: the frames queued for the
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
  With the [pipeline cache](#the-pipeline-cache-on-disk) a launch after the first loads them instead.
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

## The pipeline cache on disk

Compiling the 99 pipeline states takes about 6.5 s on the console with 6 threads (16 s seen with
fewer) and about 75 MiB of the app's memory, at every launch. `rav1g_device_create_with_cache` keeps
them in a file, and a later launch on the same adapter and driver loads them from it:

```c
struct Rav1gDevice *rav1g_device_create_with_cache(void *d3d12_device, void *d3d12_queue,
                                                   uint32_t pipeline_threads, const uint16_t *cache_dir);
```

- **Where.** `cache_dir` is the path of a folder of the app's own, as a NUL-terminated UTF-16 string:
  a UWP app passes `ApplicationData::Current().LocalCacheFolder().Path()` + `\rav1g` (cast the
  `const wchar_t *` to `const uint16_t *`). The decoder creates the folder when it is not there and
  keeps one file in it, `rav1g-pipelines-<key>.bin`; nothing else of the app should be in that folder.
  Null or an empty path is no cache: exactly `rav1g_device_create`. Files are read and written with
  path-based calls (`CreateFileW`, `ReadFile`, the standard library's file functions), which an app
  container has for its own data folders.
- **The key.** `<key>` is 16 hex digits of a fingerprint (FNV-1a 64) of everything the pipeline states
  are made of: the bytecode of every shader of the set in use (the shader pick of the DXC experiment
  changes it), the serialized root signatures, the names of the pipelines, the version of the library
  and its shader set. A new `rav1g.dll` has a new key.
- **What the file holds.** A D3D12 pipeline library (`ID3D12PipelineLibrary`) where the device has
  them (`D3D12_SHADER_CACHE_SUPPORT_LIBRARY`), otherwise the cached blob of every pipeline state
  (`GetCachedBlob`, given back in `D3D12_CACHED_PIPELINE_STATE`) where it has those
  (`D3D12_SHADER_CACHE_SUPPORT_SINGLE_PSO`); a header with the key, the size and a checksum is in front
  of either. A device with neither compiles every time ("cache unsupported").
- **Invalidation.** The file is used only when its header, size and checksum are right and the device
  takes it. A file of another driver or adapter (`D3D12_ERROR_DRIVER_VERSION_MISMATCH`,
  `D3D12_ERROR_ADAPTER_NOT_FOUND`), a damaged one (`E_INVALIDARG`, a short file, a wrong checksum) or one
  that lacks a pipeline is rebuilt: the pipelines are compiled and the file is written again. A new key
  (another library) is a miss. The file is written as a temporary file in the folder and renamed over
  its name; then the other `rav1g-pipelines-*` files of the folder are deleted, so the folder holds one
  file. The app can delete the folder at any time (`LocalCacheFolder` is not backed up or roamed).
- **Nothing of the cache fails the creation.** A folder that cannot be created, a file that cannot be
  read or written: the pipelines are compiled as without the cache, and the log line says why.
- **When.** The pipelines are still created on the `pipeline_threads` threads while the call returns.
  On a miss the file is written by the last of those threads behind the last pipeline, so
  `rav1g_device_wait_pipelines` (or the first `rav1g_decoder_create`) also waits for the write. The
  pipeline library and its blob are released then; the pipeline states stay.
- **The log line.** One per device, through the log callback, when every pipeline exists (after
  `rav1g_device_create_with_cache` has returned; the numbers here show the form, not a measurement):

  ```text
  PIPELINES cache miss: 99 pipelines in 6512 ms (pipeline library, key 89487f5980cd97ff; written: 2950 KiB in 41 ms)
  PIPELINES cache hit: 99 pipelines in 380 ms (pipeline library, key 89487f5980cd97ff)
  PIPELINES cache rebuilt (the device refused the pipeline library: hr 0x887E0002, of another driver or runtime): 99 pipelines in 6600 ms (pipeline library, key 89487f5980cd97ff; written: ...)
  PIPELINES cache unsupported (the device keeps neither pipeline libraries nor cached pipeline states): 99 pipelines in 6500 ms
  PIPELINES no cache: 99 pipelines in 6500 ms
  ```

  The time runs from the start of the device creation's pipelines (the file read included) until all
  exist; the write is behind it. "hit": every pipeline came from the file; "miss": there was no file of
  the key; "rebuilt (<reason>)": there was one and it could not be used, or not for every pipeline (the
  line then says how many were loaded). "not written: <reason>" instead of "written" when the file
  could not be written. `rav1g_device_create` writes the "no cache" line.
- **Interface version 2.** The function is new in `RAV1G_ABI_VERSION` 2 (below); `rav1g_device_create`
  is unchanged.

In Rust: `Device::with_d3d12_and_cache(device, queue, threads, Some(path))` (and `new_with_cache`).
`rav1g-dec -pipelinecache <dir>` and the test player's `-pipelinecache` use it; CI runs a stream on
WARP with a miss, a hit, a cut file and a file that is no cache at all
(`testing/conformance/pipeline_cache_check.py`).

## What the settings need

`rav1g_settings_default`, then the size and the bit depth of the sequence (from
`rav1g_peek_stream_info` on the first temporal unit with a sequence header: its largest frame size,
`max_frame_width` and `max_frame_height`, not the size of the first frame), the output callbacks,
`output_kind = RAV1G_OUTPUT_SEMI_PLANAR` and `shared_fence = true`. Leave `low_memory`,
`frames_in_parse`, `frame_slots` and `threads` at their defaults: at 3840x2160 10-bit that is 501.5
MiB of device buffers (every low memory part above 1920x1088 since package 0.2.193), two frames in
parse, three slots, two threads.

## Loading the library at run time: the interface version

`rav1g_abi_version()` returns `RAV1G_ABI_VERSION` of the `rav1g.h` the library was built with. A loader
(`LoadPackagedLibrary`, `GetProcAddress`) calls it first and refuses a library whose value is not the
`RAV1G_ABI_VERSION` of the header it was compiled with. The number is raised whenever a declaration of
the header changes (a struct, an enum, a function), not for a constant, a comment or a change inside the
library: `rav1g-header --check` (CI) fails when the declarations of the header have changed and the
number has not (`tools/rav1g-header/abi.txt` has the fingerprint of the declarations of each version).
Version 1 is the interface since package 0.2.202; a library older than that has no `rav1g_abi_version`,
and a loader that finds no such export refuses it too. Version 2 adds
`rav1g_device_create_with_cache` ([the pipeline cache](#the-pipeline-cache-on-disk)); nothing else of
version 1 changed. `rav1g_version()` is the text for a log.

## Releases

A tag `v<version>` (the version of the workspace, `Cargo.toml`, which `rav1g_version()` reports) runs
every job of a push, and when all have passed the job `release` publishes a GitHub pre-release with:

| File | What |
| --- | --- |
| `rav1g.dll` | the library of the default package: the build the console runs (the C of clang, linked by lld-link, the app runtime, FXC `cs_5_0` shaders) |
| `rav1g.dll.lib`, `rav1g.pdb` | its import library and its symbols |
| `rav1g.h` | the interface it was built with (`RAV1G_ABI_VERSION`) |
| `rav1g-licenses.zip` | the licences a redistribution carries: rav1g (BSD-2-Clause), AOM's patent licence, rav1d, the notices of the linked crates |
| `SHA256SUMS` | the SHA-256 of each, for an application that pins the release |

An application copies `rav1g.h` from the release (or from the tagged commit) and pins `rav1g.dll` by
the tag and its SHA-256. The first release is `v0.4.0-alpha.1` (2026-11-18).
