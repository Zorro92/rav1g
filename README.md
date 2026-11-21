# rav1g

A fast, open-source AV1 decoder on the GPU: entropy decoding on the CPU, everything after
it (inverse transforms, intra and inter prediction, loop filter, CDEF, loop restoration,
super-resolution, film grain) in D3D12 compute shaders. Main targets Xbox Series X|S and requires
Shader Model 6.0. The name
stands for Rust AV1 GPU: the decoder on this branch is Rust (the D3D12 host and the interface,
`crates/rav1g`, and the parser, the parse pass of rav1d in `third_party/rav1d`), HLSL
(`libav1/dx/shaders`). libaom's C parser is no longer part of the decoder; what is left of libaom in
`libav1` is the reference the shader checks of `testing/` compile. There is no C++ host on `main` any more ([docs/rust-port.md](docs/rust-port.md));
the C++ decoder it was ported from lives on in branch `cpp` (release `v0.3.0`).

Built from (measured from the CI build of `rav1g.dll`):

[![The languages rav1g.dll is built from](https://raw.githubusercontent.com/Zorro92/rav1g/badges/languages.svg)](https://github.com/Zorro92/rav1g/blob/badges/languages.json)

The bar counts the bytes of the source files that the release build of the library recorded as its
inputs (Rust, the assembly of rav1d's parse pass, the HLSL of the shaders in it; no C), written by
`testing/language_bar/language_bar.py` for every push to `main`; the JSON behind it has the numbers
and the list of files. GitHub's own language bar of this repository goes by file paths
(`.gitattributes`) and is an approximation of the same.

rav1g is a fork of AOMedia's [av1-xbox-one](https://aomedia.googlesource.com/av1-xbox-one)
(commit `b5c61563b48625c261a8d627cfa928896fd10b13`, August 2021), with upstream's history
kept. Upstream's own description, `README.pdf`, is on branches `original` and `cpp`.

## Why

Heavy 4K AV1 scenes stutter with dav1d on an Xbox Series console (~6–7 CPU cores busy), and
the console has no AV1 hardware decoding. They also choke with av1-xbox-one as shipped. A
faster GPU decoder is the remaining route to smooth heavy 4K AV1 there, and it would leave
most of the CPU free.

## Status

Experimental. The decoder is upstream's, plus timing instrumentation, a rewritten CDEF
shader and a smaller memory footprint. Tested on Xbox Series consoles in UWP app mode with
the test player (`testing/uwp_dx12_player`).

`main` targets Xbox Series X|S with D3D12 and DXC Shader Model 6.0. The revision 4 test
package compares default DXIL and DXC `-Od` per shader. The frozen FXC/Shader Model 5 line
is on branch `fxc-sm5`. [Shader pick](docs/test-player.md#shader-pick--shaderpick-the-dxc-package)

Defaults (test player and library): the rewritten CDEF shader, a decoder sized for the
stream, one back buffer shared by all frames, two frames in flight, rav1d's parse pass as
the parser, and no per-frame clears of a slot by the CPU (the wavefront of intra prediction
is kept at the edges of the tiles). Each was md5-exact on the console, except the last,
which is new in package 0.2.148.0 and md5-exact in CI; `-cpuclear` is the default before it.
The upstream behaviour was `-cdefv1 -nofitsize -lowmemparts 0
-parser c -syncclear` in the test player; `-parser c`, upstream's entropy decoder, is no longer in
the decoder (package 0.2.159). Everything else that was tried (`-slots 3`,
`-lowmem`, `-intrav2`) stays behind its flag. The columns below name the flags the runs were made
with; `-cdefv2`, `-fitsize` and the shared back buffer of `-lowmem` are the defaults now.

4K 10-bit, 1 tile, 40 Mbps, 300 frames, on the console, no presentation (ranges are over
separate runs). 24 fps needs at most 41.7 ms per frame:

| | Upstream | `-cdefv2` | `-cdefv2 -lowmem -fitsize -slots 3` |
| --- | --- | --- | --- |
| CDEF per frame | ~52 ms | 5.5–7.2 ms, md5-exact | same |
| SVT-AV1 low-delay clip | 13.4 fps, GPU busy 66.5 ms | 21.0–23.1 fps, GPU busy 30.7–32.2 ms | 25.3–25.6 fps (27.3 with `-asyncclear`, one run) |
| Jellyfin test clip | 10.5 fps, GPU busy 95.4 ms | 16.4–16.8 fps, GPU busy 50 ms (intra 21, inter 19) | 18.6–18.7 fps (19.2 with `-asyncclear`), GPU-bound at ~51 ms |

The "GPU busy" figures of the table were taken with the per-stage timing of the inter breakdown
(`-gputimedetail`), which makes the GPU slower by what its timestamps and barriers cost. Plain `-gputime`
gives, with today's decoder, 18.5 ms of GPU time per frame on the test clip of the last row (`j4k40`)
and 12.1 ms on the SVT-AV1 low-delay clip (`t40x1ld`): the decode thread, not the GPU, is what limits
both ([docs/rust-port.md](docs/rust-port.md), "Console results of step 5").

- **CDEF v2** is 7–9x faster than upstream and bit-exact. It is the default (`-cdefv1`
  selects upstream's shader).
- **Intra v2** (`-intrav2`) is bit-exact but 2.3–2.4x slower. It is a negative result, off by
  default and kept for reference.
- **Intra prediction** uses angle-specific wavefront dependencies and direct prediction for
  exact vertical and horizontal angles. `-intraod` selects the conservative wavefront with the
  DXC `-Od` kernels.
  The optimized high-bit-depth kernels use wave sums for large DC and CFL reductions.
  The default 10-bit shader prepares filtered and upsampled samples in separate ranges of
  group memory, cutting one group sync. A chroma-from-luma plane with zero alpha uses
  DC without reading luma.
  A wavefront step now waits for two loads in a row instead of four: the main shader derives
  the mode's flags from the record with arithmetic instead of a table load behind it, and loads
  its residual and its other inputs beside its edges, before the first group sync. 8-bit frames
  take the same shader (two group syncs instead of eight), with 8-bit loads and stores. Filter
  intra (both bit depths) loads all residuals of a lane at the start instead of one diagonal
  ahead.
  For one-edge angles, both bit depths wait for the opposite edge only when it supplies a missing
  primary edge. Frames without those angles use the original prefetch kernel. Use
  `-intrasingleedgelegacy` to select the original prefetch kernels and wavefront for an A/B.
  The C++ stage-dump comparison compares prediction-record contents across wavefront orders,
  and still compares every frame MD5.
- **Entropy parsing** packs AC coefficient tokens in scan order and waits only for the
  reference rows whose motion fields or segmentation map the parser reads. On the console,
  the two changes cut mean parse wall time from 32.2 to 26.2 ms on a 4K 10-bit 50 fps stream.
- **Film grain** templates are generated while the CPU records the frame (AV1 7.18.3.3).
  The GPU applies them from the frame's upload buffer, alongside its random offsets.
- **A third frame in flight** (`-slots 3`) lets CPU and GPU overlap: the decode thread's wait
  for a free slot drops from 5.6–8.3 ms to 0.3–0.6 ms per frame. It only fits in memory with
  `-lowmem`. All combinations are md5-exact. On the SVT clip the CPU (entropy decoding,
  ~34 ms on one core) is now the limit; on the Jellyfin clip the GPU is.
- **Memory**: a console app may use 1 GiB. At 4K the decoder took 882.7 MiB of it (app memory
  after creation, 2 slots), then 812.7 MiB, and 765.5 MiB sized for the stream (measured).
  The default now also shares one back buffer, 24 MiB less (computed: 615.7 MiB of D3D12
  buffers for a 4K 10-bit stream). With rav1d's parse the frames in flight take their mode
  info and coefficients from pools instead of a share for each slot: 597.6 MiB with one frame
  in parse, and 646.5 MiB with two frames in parse on three slots, which is the default at 4K
  and fits the 1 GiB (it was 916.6 MiB and did not). Since then the buffers of the stages are sized
  for the frames, the mode info grid has four bytes a unit and the mode info pool of a decoder above
  1920x1088 is half the worst case: 537.6 MiB ([docs/memory.md](docs/memory.md)). With `-lowmem` it is 663.4 MiB and with
  `-lowmem -slots 3` 773.5 MiB (measured). A decoder that
  does not fit is refused instead of losing the D3D12 device, and a destroyed decoder returns
  its memory (what stays charged to the app afterwards, up to ~250 MiB, is reused by the next decoder, and the
  player's guard counts it as available). The 1 GiB is an app that asks for nothing, which is what
  the test player is and what all of these numbers are for. An app that declares the `hevcPlayback`
  capability, as an app that plays 4K video does, is given 3.25 GB by Microsoft's documentation, in two
  pools (a general heap and a graphics partition); which of them the decoder's D3D12 buffers are
  charged to decides how much room a decoder has inside such an app. The player logs both pools
  (`MEMORY pools`) and a package variant declares the capability to measure it:
  [docs/test-player.md](docs/test-player.md#the-two-memory-pools-memory-pools-hevcplayback).
- 1080p24/30 already decodes in real time.
- **Hidden frames**: encodes with frames that are decoded but shown later (SVT-AV1 random
  access, and most real-world AV1) used to stop after the first frames, because every hidden
  frame held an output buffer. An output buffer is now taken only for a frame that is output,
  when it is output. Verified on the console: an SVT-AV1 random-access 4K clip decodes
  md5-identical to dav1d (27.9 fps at 12.6 Mbps with 2 slots, 29.4 with 3).
- **Global motion**: frames with global motion were wrong on the console in upstream (a
  shader computed block positions in double precision, which the console does not execute).
  Fixed with integer arithmetic and checked on a host against libaom
  (`testing/warp_check`). Verified on the console: 1200 frames of a 1080p stream with global
  motion, and 1080p and 4K film-grain streams with global motion and show-existing frames,
  are md5-identical to dav1d.
- **Other GPUs**: upstream's shaders are only right where the 64 threads of a thread group run
  in lockstep, as on the console; on the WARP software adapter every stream decoded wrong, and
  GPUs with waves of 32 would too. The shaders now wait for each other with group syncs, and
  host checks keep it that way (`testing/sync_check`). On WARP all 254 conformance streams
  are md5-identical to their reference (AOM's test vectors, dav1d). On the console the output is unchanged
  (md5-identical to dav1d on the 10-bit and film-grain clips) and the frame time is the same
  within noise (e.g. 27.9 → 28.5 fps and 26.0 → 25.9 fps on two 12.6 Mbps 4K clips).
- **Super-resolution**: frames with superres and CDEF and without loop restoration were upscaled
  from a width left over from an earlier frame (0 at first: every row of the frame became its
  first sample) in upstream, on every GPU. Fixed in the host code; the shader is checked on a
  host against libaom's normative upscale (`testing/superres_check`). On WARP an SVT-AV1
  stream with all superres denominators is md5-identical to dav1d, on the console too.
- **Spatial layers**: of a temporal unit the frame of the highest spatial layer is output, as
  libaom and dav1d do by default (it was every layer's frame); `AV1D_SET_OUTPUT_ALL_LAYERS`
  and `AV1D_SET_OPERATING_POINT` choose otherwise. AOM's four vectors with two spatial layers
  are md5-identical on WARP and on the console.
- **A new sequence with larger frames** (or more bits) than the decoder was created for:
  `aom_codec_decode` returns `AOM_CODEC_NEEDS_RECONFIGURE` before it decodes anything of the
  sequence, and the application creates a decoder for it and passes the temporal unit again
  ([test-player.md](docs/test-player.md#layers-and-new-sequences)). It was a decode error.
- **References of different sizes in one frame**: a warped block was clamped to the size of
  the frame's first reference instead of its own, and a block with global motion was warped
  from a scaled reference, which libaom and the specification do not do. Both fixed and
  checked on a host against libaom (`testing/warp_check`); the console output is unchanged.
- **8-bit streams on the console** (fixed, rav1g 709cf04; verified on the console with package
  0.2.31.0: every 8-bit stream we have is md5-identical to dav1d, the 10-bit set unchanged, frame
  times unchanged). Before the fix they were md5-identical to dav1d on WARP and wrong on the console: luma off by ±1–2 on about 0.4% of the pixels
  from the first (key) frame on, chroma exact, 10-bit streams not affected. The console's driver
  compiles one instruction of the 8-bit vertical loop filter wrongly: in the 8-tap branch FXC emitted
  `iadd r2.x, r3.w, -r3.y` and, four instructions later, `iadd r3.yw, r3.wwww, -r4.xxxy`, which
  rewrites those two components in place, and the console ran the first as `r3.z - r3.x`. A model of
  the filter with that one change gives the console's output of a 3840x2160 frame to the last sample
  (40240 edges in that branch); the rest of the two 8-bit filters runs as written. The branch is
  rewritten to avoid the pair. WARP's output was unchanged. The FXC listing scan is on
  `fxc-sm5`. [Details of the fix](docs/gpu-av1-decoder.md#a-paired-read-before-an-overlapping-in-place-write-2026-11-12)
- **Output that depended on what the process had decoded before** (fixed, rav1g dda14c7; found and
  verified on WARP, not yet run on the console). A decoder created behind another one in the same
  process could be wrong from its first frame on and right alone: `av1-1-b10-00-quantizer-00` behind
  any 8-bit quantizer vector every time, `av1-1-b8-22-svc-L2T1-2` behind `svc-L2T1` now and then,
  which had passed for a flake of slow CI runners. `gen_pred_blocks.hlsl` read the filter-intra flag
  of a block together with two bytes of struct padding next to it; a first decoder finds zeros
  there, a later one what the memory of a destroyed decoder held. The read is masked (one
  instruction of the shader changes), every other read of the mode info was checked against the
  struct's layout (no second case), and CI now decodes these sequences in one process without
  decoding anything again alone, and 137 streams with the mode info filled with 0xA5 when the
  decoder is created (`-poisonmi`)
  ([gpu-av1-decoder.md](docs/gpu-av1-decoder.md#padding-of-the-mode-info-record-2026-11-12)).
- **A load at an address made of stale constants** (fixed, rav1g 25c85ca; found and verified on
  WARP, not yet run on the console). For a plane without loop restoration the host left the unit
  fields of the plane's constants unwritten, and `loop_restoration.hlsl` computed the address of a
  table entry from them and loaded it without a branch (FXC had turned the `if` into a load and a
  select). The constants of a frame are in upload memory that is only rewound between frames, so
  the fields held what an earlier frame had left there (110 of 346 such records in a run of 136
  streams), and the table is a root SRV, without bounds checking: a load outside of mapped memory
  removes the D3D12 device. The output was never wrong. The host now writes a valid unit grid and
  the shader loads under a branch, for restored planes only. It showed with every CPU-written
  buffer filled with 0xA5 (`-poisonbuf`), which the decoder failed and now passes; the flag also
  fills the constants of every stage again in every frame, and CI decodes 137 streams with it on
  every push. This read is the candidate for the D3D12 device removals seen now and then on WARP:
  the conformance runs print a count of them to tell
  ([gpu-av1-decoder.md](docs/gpu-av1-decoder.md#unwritten-fields-of-a-constant-record-2026-11-12)).
- **Every stream we test is md5-identical to dav1d on the console and on WARP**: 254 conformance
  streams on WARP; on the console the 8-bit and 10-bit clips from 66x66 to 4K, hidden frames, film
  grain, global motion, spatial layers, size changes, scaled references and superres.
  The 10-bit stream with scaled references that stood here as wrong on the console is right: its md5
  references had been made with ffmpeg scaling every frame to the size of the first.
- **Conformant on the Argon Profile 0 set** (2026-11-20, see [References](#references) and
  [testing/argon/RESULTS.md](testing/argon/RESULTS.md)): all 765 valid streams within the decoder's
  4096x2176 limit decode bit-exact on WARP, every frame against the expected md5s, and all 114 invalid
  streams are decoded or rejected without a crash, hang or device loss. The 212 streams with larger
  frames are outside the decoder's limit; Profiles 1 and 2 (4:4:4, 12-bit) are out of scope. The bugs
  the set found came from the code rav1g was forked from: palette blocks without a residual, a sample
  of 10-bit intra block copy, overlapped prediction from 128-sample neighbours, film grain chroma on
  odd sizes, restoration and output buffers too small for 32x32 chroma restoration units and narrow
  pictures, and mode info pools too small for the densest frames (they now grow when a frame needs
  it, and normal video uses the same memory as before). CI runs the set on request (`-f argon=true`).

**The Rust port, phase 1, is complete** ([docs/rust-port.md](docs/rust-port.md), 2026-11-12 to
2026-11-13): the whole D3D12 host is Rust, from the creation of a decoder to the completion thread, with
a Rust interface (the `rav1g` crate) and an interface for C (`crates/rav1g/include/rav1g.h`, which the
test player uses). On the console it decodes the md5 set byte-identically and is as fast as the C++
decoder or faster (1.5 to 4 % in the tile threads with the C of the parser compiled by clang, 7 to 14 %
in the recording). The C++ host was removed from this branch after that; CI still holds every frame,
and the stage dumps of the Rust decoder against the C++ decoder of branch `cpp`, for as
long as the two have the same shaders. The test player comes in two packages per run: the default with
the library as `rav1g.dll`, and a build with the static library linked by MSVC
([docs/test-player.md](docs/test-player.md)).

**Phase 2, the parser of rav1d in place of the C parser, is complete**
([docs/phase2-rav1d.md](docs/phase2-rav1d.md) has the design, the measurements and the progress, step
by step). The decoder parses with rav1d's parse pass (`third_party/rav1d`), with two frames in parse
on two threads for a 4K stream, and takes the mode info records, coefficients and transform block
records of all frames in flight from pools; a 4K 10-bit decoder is 646.5 MiB of device buffers and
runs in a console app of 1 GiB at 45 to 49 frames per second on the two heavy 4K clips (19.5 and 22.7
with the C parser). In its last step the entropy decoder of libaom left the decoder: the library
compiles no C of `libav1` any more (the eight set-up functions and the tables the host still needed of
libaom's common code are Rust now), `-parser c`, `Settings::parser` and `RAV1G_PARSER_*` are gone, and
the C parser lives on in one place only, as a reference: the C++ decoder of branch `cpp`, which CI
holds every frame and the stage dumps against. Its sources are no longer on `main`: of libaom's C the
tree keeps the headers and the 17 files the shader checks compile or the build reads tables from.
`tools/rav1g-parse-check` holds rav1d's parse with any threads, frames in parse and switches against
its parse with one thread, value by value, on Linux in CI. Behind it, the semi-planar output
([docs/output.md](docs/output.md)): the output pass can write a frame into one NV12 or P010 texture of
the application, a D3D11 texture opened in D3D12 among them, with one output fence a decoder that
D3D11 can wait on. Next:
inter and intra prediction on the GPU (a measured `-interfast` prototype showed that inter time is per
block and per dispatch, not filtering; see [gpu-av1-decoder.md](docs/gpu-av1-decoder.md)).
The goals, in order: heavy 4K at 30 fps in real time (at most 33.3 ms per frame; met on the
two heavy clips since the frames in parallel of phase 2), then faster still, then decoding with as much of the CPU, GPU and
memory as possible left to the application.

## Documentation

- [docs/test-player.md](docs/test-player.md): building, installing and running the test
  player, its defaults and flags, log lines, md5 checks (references can be made with dav1d:
  `testing/md5ref/make_md5_ref.sh`) and the host-side shader checks.
- [docs/gpu-av1-decoder.md](docs/gpu-av1-decoder.md): history, measurements and reasoning.
- [docs/warp-decoder.md](docs/warp-decoder.md): `rav1g-dec`, the decoder as a console program on the
  WARP software adapter, the conformance run of every push, and the comparison with `warp_decoder`,
  the same program of the C++ decoder.
- [docs/rust-port.md](docs/rust-port.md): the Rust port, phase 1: its steps, gates and progress, what
  was removed at its end and what is open. The workspace builds with
  `RAV1G_SHADER_DIR=<shader build> cargo build --release` on Windows and with
  `cargo xwin build --target x86_64-pc-windows-msvc` on Linux.
- [docs/memory.md](docs/memory.md): what streams really use of the decoder's memory (`-memstats`), and
  the reductions that follow from it, ranked.
- [docs/output.md](docs/output.md): the output images. By default a buffer with the planes of a frame
  and one texture a plane; with `Rav1gSettings::output_kind = RAV1G_OUTPUT_SEMI_PLANAR` one NV12 or
  P010 texture that the decoder's output pass writes, which an application can share with a D3D11
  device, and the output fence that says when a frame is written. With a D3D11 consumer sketch and an
  integration note.
- [docs/color.md](docs/color.md): what the decoder reports about the colour of a stream (the colour
  description of the sequence header, the HDR metadata: mastering display and content light level),
  in `rav1g.h` and in the Rust interface, and what an application that shows the frames does with
  each value.
- [docs/phase2-rav1d.md](docs/phase2-rav1d.md): the design of phase 2, rav1d's parser in place of the
  C parser, and its progress.
- [third_party/rav1d/PATCHES.md](third_party/rav1d/PATCHES.md): the copy of rav1d, what was changed
  against upstream and how to check it.

## References

What decides whether rav1g decodes correctly, in this order:

1. **The AV1 specification**, the normative definition of every output sample: the AV1 Bitstream
   and Decoding Process Specification, version 1.0.0 with Errata 1, from the Alliance for Open Media
   ([specifications page](https://aomedia.org/specifications/av1/)). Its source is Markdown, one file
   per section, at the release tag
   [`v1.0.0-errata1` of AOMediaCodec/av1-spec](https://github.com/AOMediaCodec/av1-spec/releases/tag/v1.0.0-errata1).
   Cite sections from that tag: the repository's `master` has edits made after the errata.
2. **Argon Streams AV1**, AOMedia's conformance suite for that specification
   ([AV1 video decoder verification tool](https://aomedia.org/av1-video-decoder-verification-tool/)):
   streams that cover every syntax element and decoding path, with expected output md5s made by
   libaom at tag `v1.0.0-errata1`. rav1g is checked against the Profile 0 part (8- and 10-bit
   4:2:0). The generator and coverage tool are at
   [AOMediaCodec/argon-streams](https://gitlab.com/AOMediaCodec/argon-streams).
3. **dav1d**, an independent decoder that passes Argon, makes the md5 references of the clips that have
   no conformance md5 of their own (`testing/md5ref/make_md5_ref.sh`).

[av1-xbox-one](https://aomedia.googlesource.com/av1-xbox-one), which rav1g is forked from, is not a
reference: where its code and the specification disagree, the code is wrong.

## Licence

BSD-2-Clause (`LICENSE.txt`, `libav1/LICENSE`) plus the Alliance for Open Media Patent
License 1.0 (`libav1/PATENTS`). `third_party/libwebm` carries its own licence and patent
notices (`third_party/libwebm/LICENSE.TXT`, `third_party/libwebm/PATENTS.TXT`).
`third_party/rav1d` is the parse pass of [rav1d](https://github.com/memorysafety/rav1d), the Rust
port of dav1d (BSD-2-Clause, copyright VideoLAN, the dav1d authors and the Internet Security Research
Group: `third_party/rav1d/COPYING`, `third_party/rav1d/doc/PATENTS`). The crates of crates.io
that are linked into the library (rav1d's dependencies and the Windows bindings) are MIT or
Apache-2.0 licensed; their notices are collected in `third_party/rust-crates/NOTICES.txt`. Keep all of
them when redistributing source or binaries; CI artifacts include them under `licenses/`.

## Disclaimers

Not affiliated with or endorsed by Microsoft. Xbox is a trademark of Microsoft Corporation.

Independent project, not affiliated with rav1e or rav1d; it uses rav1d's parser under rav1d's licence.

Not affiliated with or endorsed by the Alliance for Open Media.
