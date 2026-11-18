# Output images

Where the decoder puts a frame, and how an application takes it. Two kinds, chosen when the decoder is
created (`Rav1gSettings::output_kind`, `Settings::output_kind`):

| Kind | An image is | For |
| --- | --- | --- |
| `RAV1G_OUTPUT_PLANES` (the default) | a buffer with the planes of the frame one behind the other, one texture a plane (R8 or R16), or both | reading frames on the CPU; a renderer with one texture a plane (the test player) |
| `RAV1G_OUTPUT_SEMI_PLANAR` | **one `DXGI_FORMAT_NV12` texture (8-bit frames) or one `DXGI_FORMAT_P010` texture (10-bit frames)** | an application whose video path takes that texture, a D3D11 one among them |

The decoder converts nothing in either: the samples are those of the stream (film grain applied), and
what they mean is in the colour description of every frame ([color.md](color.md)). Tone mapping is the
application's.

The first kind is what the decoder always had and is not described again here (`rav1g.h`,
`Rav1gImage`). The rest of this page is the second.

- [The image](#the-image)
- [Who writes the texture](#who-writes-the-texture)
- [States](#states)
- [The output fence](#the-output-fence)
- [A D3D11 consumer](#a-d3d11-consumer)
- [Colour](#colour)
- [What cannot be had](#what-cannot-be-had)
- [Cost](#cost)
- [Checks](#checks)
- [Integration note for an FFmpegInteropX-side developer](#integration-note-for-an-ffmpeginteropx-side-developer)

## The image

With `output_kind = RAV1G_OUTPUT_SEMI_PLANAR` the decoder asks for an image with
`Rav1gImageRequest::format` `RAV1G_IMAGE_FORMAT_NV12` (an 8-bit frame) or `RAV1G_IMAGE_FORMAT_P010` (a
10-bit frame), and the application answers with one texture in `Rav1gImage::d3d12_textures[0]` (the
other two stay null):

| | |
| --- | --- |
| Format | `DXGI_FORMAT_NV12` for `RAV1G_IMAGE_FORMAT_NV12`, `DXGI_FORMAT_P010` for `RAV1G_IMAGE_FORMAT_P010` |
| Size | `width` and `height` of the request, **both rounded up to even** (a 4:2:0 texture has no odd size) |
| Mip levels, array size, samples | 1, 1, 1 |
| Flags | `D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS` for the direct path (below); none is fine too |
| State | `D3D12_RESOURCE_STATE_COMMON` when `get_image` returns |
| Where it comes from | anywhere: a committed resource of the application, one it created shareable, or one it opened from a D3D11 texture through a shared handle. The decoder does not care |

A texture of another format or size fails the temporal unit, with an `OUTPUT` line in the log that
names what was asked and what came.

What the decoder writes:

- **Plane 0, luma**: one sample a texel. NV12: the 8 bits. P010: 16 bits with the 10 bits of the sample
  in the high bits, that is the sample times 64; the low six bits are 0.
- **Plane 1, chroma**, half the width and half the height: U and V of a sample side by side, U first,
  each as luma has its samples.
- **A frame of odd width** has one luma column more in the texture than it has samples: it repeats the
  last column. **A frame of odd height** has one luma row more: it repeats the last row. The chroma
  plane of such a frame is whole without padding: a 4:2:0 frame of 225x225 has 113x113 chroma samples,
  which is the chroma plane of a 226x226 texture. So the texture is filled to its last texel, and a
  renderer that filters across the edge of the frame reads the edge and not something else. The frame
  itself is `Rav1gFrame::width` by `height`; crop to that.
- **A monochrome stream** gets the same texture, with the middle value in the chroma plane (128; 512
  times 64 in P010), so it is shown grey by a renderer that does not know. `Rav1gFrame::monochrome` says
  so.

`Rav1gFrame::format` says which of the two the frame is in; `bit_depth` is 8 or 10 as before.

A **buffer** works with this kind too (`d3d12_buffer`, alone or next to the texture): it gets the two
planes one behind the other as the texture has them in memory, rows of a multiple of 256 bytes, the
chroma plane at a multiple of 512; `Rav1gFrame::planes[0]`, `planes[1]` and `strides` say where
(`planes[2]` is null). `Rav1gImageRequest::buffer_size` is its size. That is NV12 or P010 for a reader on
the CPU.

## Who writes the texture

The output pass of the decoder is one pass at the end of the frame's command list: film grain, where
the frame has it, or a copy shader for the planes that get none. With the planes kind it puts the frame
together in the decoder's back buffer and D3D12 copies from there into the application's buffer or
textures. With the semi-planar kind there are two paths, chosen for every image:

- **Direct.** The shaders of the pass write the two planes of the application's texture themselves,
  through a view of each plane (`R8_UINT` and `R8G8_UINT` for NV12, `R16_UINT` and `R16G16_UINT` for
  P010; `D3D12_TEX2D_UAV::PlaneSlice` 0 and 1). Nothing is put together in the back buffer and nothing
  is copied: the texture is written once, by the pass that computes the samples. Taken when the texture
  was created with `D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS`, the device says that it has typed
  unordered access views on the format and on the two view formats
  (`D3D12_FEATURE_FORMAT_SUPPORT`, `D3D12_FORMAT_SUPPORT1_TYPED_UNORDERED_ACCESS_VIEW`), and the image
  has no buffer next to the texture.
- **Staged.** The same shaders put the frame together in the back buffer in the layout of the texture,
  and two copies (`CopyTextureRegion`, one a plane) move it into the texture. This is what the planes
  kind costs, with two copies in place of three. Taken for every other texture, so for any NV12 or
  P010 texture a device can create, and for an image with a buffer.

Which one a decoder takes is in its log: `OUTPUT semi-planar: NV12 textures yes, written by shaders
yes; P010 textures ...` at its creation (what the device says), and `OUTPUT P010 direct: ...` or
`OUTPUT P010 staged: ...` with the first frame and whenever the path changes.

A D3D11 texture is one shaders may write when it was created with `D3D11_BIND_UNORDERED_ACCESS`; opened
in D3D12 it then has `D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS`. A driver that refuses that bind flag
for NV12 or P010 leaves the staged path, which needs nothing of the texture.

The decoder needs no memory for this kind beyond one descriptor heap of 64 descriptors. The
application's texture takes the place of its three plane textures and is about their size together.

## States

| When | State of the texture |
| --- | --- |
| `get_image` returns | `D3D12_RESOURCE_STATE_COMMON` (the application's duty) |
| between `get_image` and `frame` | the decoder's: unordered access (direct) or copy destination (staged), by transitions of its own command list. The application does not use the texture |
| `frame` is called | `D3D12_RESOURCE_STATE_COMMON` again, written, and the application's |

The common state is what a texture has to be in when another queue, device or API uses it, so a
texture that D3D11 also holds needs nothing more. An application that renders the texture on the same
D3D12 device makes its own transition from the common state and back before it gives the texture to the
decoder again.

## The output fence

A decoder has one output fence (`rav1g_decoder_output_fence`, an `ID3D12Fence *`; it exists from the
creation on and lives as long as the decoder). The queue signals it behind every command list of the
decoder with the next number. `Rav1gFrame::d3d12_fence` is that fence and `Rav1gFrame::fence_value` is
the value it has when the list that wrote the frame has run. The values grow from frame to frame.

The `frame` callback is unchanged: it is called when the GPU has written the frame, so when the
application sees a value, the fence has reached it. On the D3D12 device of the decoder there is nothing
to wait for.

It is there for the other side of a shared texture. A D3D11 device has a queue of its own, and what it
knows about the D3D12 queue is what a fence tells it. With `Rav1gSettings::shared_fence` the fence is
created with `D3D12_FENCE_FLAG_SHARED`; the application makes a handle of it once
(`ID3D12Device::CreateSharedHandle`), opens it in D3D11 (`ID3D11Device5::OpenSharedFence`), and before
it uses the texture of a frame in D3D11 it has the context wait for the frame's value
(`ID3D11DeviceContext4::Wait`). That wait is on the GPU's side: the CPU does not block.

Up to 0.2.159 `Rav1gFrame::d3d12_fence` was the fence of the pipeline slot that decoded the frame, one
of up to six with values of their own. It is one fence now, in every output kind.

## A D3D11 consumer

What the test player's `-interop` mode does, and what an application with a D3D11 video path does
(`testing/player_common/semi_planar.cpp` is this, with every result checked and logged):

```cpp
// Once. The D3D11 device has to be on the adapter of the D3D12 device.
ComPtr<ID3D11Device5> d3d11;            // QueryInterface of the app's ID3D11Device
ComPtr<ID3D11DeviceContext4> context;   // QueryInterface of its immediate context

Rav1gSettings settings;
rav1g_settings_default(&settings);
settings.output_kind = RAV1G_OUTPUT_SEMI_PLANAR;
settings.shared_fence = true;
// ... size, bit depth, callbacks ...
rav1g_decoder_create(device, &settings, &decoder);

HANDLE handle;
d3d12->CreateSharedHandle((ID3D12Fence*)rav1g_decoder_output_fence(decoder), nullptr, GENERIC_ALL, nullptr, &handle);
ComPtr<ID3D11Fence> fence11;
d3d11->OpenSharedFence(handle, IID_PPV_ARGS(&fence11));
CloseHandle(handle);

// For every image of the pool: a D3D11 texture, opened in D3D12.
D3D11_TEXTURE2D_DESC desc = {};
desc.Width = (request->width + 1) & ~1u;
desc.Height = (request->height + 1) & ~1u;
desc.MipLevels = desc.ArraySize = 1;
desc.Format = request->format == RAV1G_IMAGE_FORMAT_P010 ? DXGI_FORMAT_P010 : DXGI_FORMAT_NV12;
desc.SampleDesc.Count = 1;
desc.Usage = D3D11_USAGE_DEFAULT;
desc.BindFlags = D3D11_BIND_SHADER_RESOURCE | D3D11_BIND_UNORDERED_ACCESS;  // without UNORDERED_ACCESS: staged
desc.MiscFlags = D3D11_RESOURCE_MISC_SHARED_NTHANDLE | D3D11_RESOURCE_MISC_SHARED;
ComPtr<ID3D11Texture2D> texture11;
d3d11->CreateTexture2D(&desc, nullptr, &texture11);
ComPtr<IDXGIResource1> shared;
texture11.As(&shared);
shared->CreateSharedHandle(nullptr, DXGI_SHARED_RESOURCE_READ | DXGI_SHARED_RESOURCE_WRITE, nullptr, &handle);
ComPtr<ID3D12Resource> texture12;
d3d12->OpenSharedHandle(handle, IID_PPV_ARGS(&texture12));
CloseHandle(handle);
// Keep both. An opened texture is in the common state.

// get_image: give the D3D12 side.
image->d3d12_textures[0] = texture12.Get();
image->token = entry;   // finds texture11 again

// frame: the D3D11 side waits, then uses its texture.
context->Wait(fence11.Get(), frame->fence_value);
// texture11 now has the frame: a shader resource view of each plane (R8/R8G8 or R16/R16G16), a video
// processor input, a copy. Crop to frame->width x frame->height; frame->color says what the samples mean.
```

The image is the application's again when `frame` returns, and the decoder holds no reference of it
after that; give it back to the decoder (from `get_image`) when D3D11 is done with it. Between
`get_image` and `frame` D3D11 must not use the texture.

## Colour

Nothing changes: `Rav1gFrame::color` (primaries, transfer, matrix, range, chroma sample position) and
the HDR10 metadata of the frame are what they are with the planes kind ([color.md](color.md)). For this
kind in particular:

- P010 of an HDR10 stream is BT.2020 primaries, PQ, BT.2020 NCL, limited range, the 10 bits in the high
  bits: what `DXGI_COLOR_SPACE_YCBCR_STUDIO_G2084_LEFT_P2020` (or `TOPLEFT`, by the chroma sample
  position) names for a video processor or a swap chain. HLG is `YCBCR_STUDIO_GHLG_TOPLEFT_P2020`.
- NV12 of an SDR stream is `YCBCR_STUDIO_G22_LEFT_P709` when the stream says BT.709 or nothing (or the
  `P601` one by `matrix_coefficients`); `full_range` selects the `FULL` ones.
- A 10-bit SDR stream is P010 too; an 8-bit stream is NV12 whatever its transfer says.
- A shader that reads the P010 planes as `R16_UNORM` gets the sample times 64 over 65535, which is the
  sample over 1023 to within 0.1 %.

## What cannot be had

- **4:4:4 and 4:2:2 streams**: the decoder decodes 4:2:0 and monochrome only; such a stream is
  `RAV1G_STATUS_UNSUPPORTED_BITSTREAM` at its first frame, in every output kind.
- **12-bit streams**: not decoded (as before).
- **`packed_10x3`** means nothing with the semi-planar kind.
- **Presenting in the test player**: its display reads one texture a plane, so `-output p010` and
  `-interop` are headless (`-nopresent` is implied).
- **A device without NV12 or P010 textures**: the application cannot create the image; the decoder is
  created all the same (its log says `textures no`) and can still write the two planes into a buffer.

Known and left as it is, because it is the same in both kinds and no stream of the test sets has it:
film grain on a frame of **odd** size. The planes kind does not write the last chroma row of a frame of
odd height with chroma grain (the filter shader bounds chroma by `height >> 1`), and neither kind
repeats the last luma sample for the chroma scaling of the last column or row, as the specification
does. The semi-planar kind writes the row; so on such a stream the two kinds differ in it.

## Cost

The pass is the same pass: no dispatch more, and on the direct path no copy and no use of the back
buffer. The decoder allocates one descriptor heap of 64 descriptors for this kind and nothing else; the
application's one texture is the size of the three plane textures it replaces.

What it costs in time was measured on WARP only so far (CI run 43627226893, job `rust-output`,
`testing/conformance/output_cost.py`: `rav1g-dec -gputime`, the median of a frame in ms). **WARP is a
CPU rasterizer: this says how the kinds compare there and nothing about a GPU.** The console lines of
[test-player.md](test-player.md#console-the-task-lines-1) are the measurement that counts.

| Stream | Images | Path | `film_grain` | `copy_plane` | `output_copy` | The three together |
| --- | --- | --- | ---: | ---: | ---: | ---: |
| 1920x1080 10-bit, no grain | planes | | | 2.52 | 2.13 | 4.64 |
| | `-output p010` | direct | | 11.04 | 0.00 | 11.04 |
| | `-output p010-copy` | staged | | 7.43 | 0.90 | 8.32 |
| | `-output p010-buffer` | staged | | 7.41 | 2.00 | 9.41 |
| 352x288 10-bit, film grain | planes | | 14.27 | | 0.16 | 14.43 |
| | `-output p010` | direct | 13.89 | | 0.00 | 13.89 |
| | `-output p010-copy` | staged | 13.64 | | 0.07 | 13.70 |
| | `-output p010-buffer` | staged | 13.79 | | 0.12 | 13.90 |
| 352x288 8-bit, film grain | planes | | 12.71 | | 0.08 | 12.79 |
| | `-output p010` | direct | 12.57 | | 0.00 | 12.57 |
| | `-output p010-copy` | staged | 12.29 | | 0.06 | 12.36 |
| | `-output p010-buffer` | staged | 12.13 | | 0.08 | 12.21 |

How to read it:

- **`output_copy` is gone on the direct path** (0.001 ms: two state changes), and smaller on the staged
  path into a texture (two copies in place of three).
- **With film grain** the pass costs what it did, a little less: the same shader computes the same
  samples and stores them elsewhere.
- **Without film grain, on WARP, the semi-planar copy shader is slower than the copy of the planes
  kind**: 11.0 ms against 2.5 + 2.1 for a 1080p 10-bit frame on the direct path, 7.4 + 0.9 staged. The
  planes kind copies 16 bytes a thread as they are; the semi-planar shader takes the samples apart,
  shifts them and puts U and V side by side, and on the direct path stores sixteen texels a thread
  through typed views, each of which a CPU rasterizer bounds-checks. Whether a GPU shows any of this is
  what the console timing has to say; the stages `copy_plane` and `output_copy` of its `-gputime` lines
  are the comparison. If it does, the staged shader can copy whole words where the frame has no
  padding (a 10-bit luma row is its words shifted by six), which this version does not do.

**On the console** (package 0.2.161.0, the base 1 GiB, slow state, the owner's app not running; the
timing lines of [test-player.md](test-player.md#console-the-task-lines-1), 300 frames, no film grain
in either clip; the means of a frame in ms):

| Clip | Images | Path | `copy_plane` | `output_copy` | Together | GPU busy | fps |
| --- | --- | --- | ---: | ---: | ---: | ---: | ---: |
| `j4k40` (3840x2160 10-bit) | planes | | 0.65 | 0.97 | 1.62 | 18.86 | 45.6 |
| | `-output p010` | direct | 1.27 | 0.02 | **1.29** | 18.56 | 46.1 |
| | `-output p010-copy` | staged | 0.79 | 1.36 | 2.16 | 19.42 | 44.9 |
| | `-interop` | direct | 1.26 | 0.02 | 1.29 | 18.55 | 45.5 |
| `d2` (3840x2160 10-bit) | planes | | 0.65 | 0.98 | 1.62 | 10.36 | 48.8 |
| | `-output p010` | direct | 1.26 | 0.02 | **1.29** | 10.04 | 48.9 |
| | `-output p010-copy` | staged | 0.78 | 1.37 | 2.15 | 10.89 | 48.5 |
| | `-interop` | direct | 1.26 | 0.02 | 1.28 | 10.04 | 49.2 |

- WARP's slowdown does not carry over: on the console's GPU the direct path costs 0.33 ms a 4K frame
  **less** than the planes kind (1.29 against 1.62), and that is what a D3D11 texture with
  `BIND_UNORDERED_ACCESS` gets (`-interop`). The staged path costs 0.5 ms more than the planes kind.
- The fps are the same in every kind: at 4K the parse sets the pace, not the GPU.
- The word-wise staged copy is not needed for the media app, which can create its textures with
  `BIND_UNORDERED_ACCESS`.

Console checks of the same package: the md5 set of 15 streams with `-output p010` and with
`-output p010-copy`, one launch each: 30 of 30. `-interop` on `d2` (300 frames) and `j4k40` (the 300
frames of its reference): `INTEROP is OK` and `MD5 is OK` on both; the D3D11 device opened the
decoder's shared fence, created the P010 textures with `D3D11_RESOURCE_MISC_SHARED_NTHANDLE | SHARED`
and `UNORDERED_ACCESS`, D3D12 opened them as writable, and every frame was waited for on the shared
fence and read from the D3D11 texture. So the interop works in a UWP app on the console.

## Checks

- `rav1g-dec -output p010` (`nv12` is the same word) gives the decoder NV12 and P010 textures that
  shaders may write, `-output p010-copy` textures they may not, `-output p010-buffer` buffers. It copies
  every texture back and hashes the samples as the planes are hashed (the 10 bits out of the high bits,
  U and V taken apart), so **the md5 references are those of the planes kind**: byte identity is one md5
  a frame. It also fails a frame whose low six bits are not zero, whose padding does not repeat the
  edge, or whose monochrome chroma is not the middle value.
- In every run of `rav1g-dec`, in every output kind and both interfaces, a frame fails the task when
  the output fence has not reached the frame's value or the value is not above that of the frame
  before. `-sharedfence` creates the fence shareable, opens it through a shared handle and holds the
  opened fence to the same.
- Since 2026-11-18, in every run of `rav1g-dec` too: each decode call passes the number of its temporal
  unit, from 1, as its user data, and a frame fails the task when the user data it brings back
  (`Frame::user_data`, `Rav1gFrame::user_data`) is 0 or not above that of the frame before (equal is
  allowed with `-alllayers`). So an application can carry a presentation time through the decoder, and
  the order and the pairing of the values are held on every stream of every run.
- CI, job `rust-output`: the whole conformance set with `-output p010`, with `-capi -output p010-copy
  -sharedfence` and with `-output p010-buffer`, against the references and `expected.txt`; three streams
  with `-output p010` under GPU-based validation; the player's decoder code with `-output p010` and with
  `-interop` (`rav1g-dec -playersmoke`); the cost table.
- The test player: `-output p010`, `-output p010-copy` and `-interop`
  ([test-player.md](test-player.md#semi-planar-output-and-d3d11-interop--output--interop)).

## Integration note for an FFmpegInteropX-side developer

What changes on the consumer's side, in the order to do it:

1. **Header.** Take `rav1g.h` of 0.2.160 or later. `Rav1gSettings` has two fields more
   (`output_kind`, `shared_fence`, behind `memory_headroom`) and `Rav1gFrame` one (`format`, at its end):
   compile against the new header, keep filling the settings with `rav1g_settings_default`.
2. **Devices.** The D3D12 device given to `rav1g_device_create` and the D3D11 device of the video path
   have to be on the same adapter (create the D3D12 device on the adapter of the D3D11 device:
   `IDXGIDevice::GetAdapter`). The D3D11 device needs `ID3D11Device5` and its context
   `ID3D11DeviceContext4` (Windows 10 1703).
3. **Settings.** `output_kind = RAV1G_OUTPUT_SEMI_PLANAR`, `shared_fence = true`. After
   `rav1g_decoder_create`: open `rav1g_decoder_output_fence` in D3D11, once for each decoder (a decoder
   that is replaced for a new sequence has a new fence).
4. **Pool.** One D3D11 texture an image, NV12 or P010 by `Rav1gImageRequest::format`, of the request's
   size rounded up to even, `D3D11_RESOURCE_MISC_SHARED_NTHANDLE | D3D11_RESOURCE_MISC_SHARED`, bind
   flags `SHADER_RESOURCE | UNORDERED_ACCESS` (try without `UNORDERED_ACCESS` if the driver refuses:
   the decoder then copies into it, which costs what the plane textures cost today); open each in D3D12
   once and keep the pair. Count the pool in what `memory_available` holds back, as today.
5. **`get_image`.** `d3d12_textures[0]` = the D3D12 side of a free pair, `token` = the pair. The texture
   must not be in use by D3D11.
6. **`frame`.** `context->Wait(fence11, frame->fence_value)` on the thread that owns the D3D11 context
   (the callback comes on a thread of the decoder: queue the frame to that thread, as a decoder that
   returns `AVFrame`s would), then hand the D3D11 texture on as the frame's surface, with
   `frame->width`/`height` as the visible size, `frame->color` and the HDR metadata as today.
7. **Recycle.** When the renderer has released the surface, the pair is free for `get_image` again.
8. **Check.** The decoder's log has one `OUTPUT ... direct` or `OUTPUT ... staged` line: direct is the
   one without a copy. The test player's `-interop` task on the same console says whether the console's
   driver creates the textures with `UNORDERED_ACCESS` and whether the fence opens
   (`INTEROP ...` lines).
