# rav1g

A fast, open-source AV1 decoder on the GPU: entropy decoding on the CPU, everything after
it (inverse transforms, intra and inter prediction, loop filter, CDEF, loop restoration,
super-resolution, film grain) in D3D12 compute shaders. It runs on any D3D12 GPU. The name
stands for Rust AV1 GPU: the C++ host is being ported to Rust on this branch
([docs/rust-port.md](docs/rust-port.md)); the C++ decoder stays on branch `cpp` (release `v0.3.0`).

rav1g is a fork of AOMedia's [av1-xbox-one](https://aomedia.googlesource.com/av1-xbox-one)
(commit `b5c61563b48625c261a8d627cfa928896fd10b13`, August 2021), with upstream's history
kept. Upstream's own description is in `README.pdf`.

## Why

Heavy 4K AV1 scenes stutter with dav1d on an Xbox Series console (~6–7 CPU cores busy), and
the console has no AV1 hardware decoding. They also choke with av1-xbox-one as shipped. A
faster GPU decoder is the remaining route to smooth heavy 4K AV1 there, and it would leave
most of the CPU free.

## Status

Experimental. The decoder is upstream's, plus timing instrumentation, a rewritten CDEF
shader and a smaller memory footprint. Tested on Xbox Series consoles in UWP app mode with
the test player (`testing/uwp_dx12_player`).

Defaults (test player and library): the rewritten CDEF shader, a decoder sized for the
stream, one back buffer shared by all frames, two frames in flight. Each was md5-exact on
the console; the upstream behaviour is `-cdefv1 -nofitsize -lowmemparts 0` in the test
player. Everything else that was tried (`-slots 3`, `-lowmem`, `-asyncclear`, `-intrav2`,
`-intraopt`) stays behind its flag. The columns below name the flags the runs were made
with; `-cdefv2`, `-fitsize` and the shared back buffer of `-lowmem` are the defaults now.

4K 10-bit, 1 tile, 40 Mbps, 300 frames, on the console, no presentation (ranges are over
separate runs). 24 fps needs at most 41.7 ms per frame:

| | Upstream | `-cdefv2` | `-cdefv2 -lowmem -fitsize -slots 3` |
| --- | --- | --- | --- |
| CDEF per frame | ~52 ms | 5.5–7.2 ms, md5-exact | same |
| SVT-AV1 low-delay clip | 13.4 fps, GPU busy 66.5 ms | 21.0–23.1 fps, GPU busy 30.7–32.2 ms | 25.3–25.6 fps (27.3 with `-asyncclear`, one run) |
| Jellyfin test clip | 10.5 fps, GPU busy 95.4 ms | 16.4–16.8 fps, GPU busy 50 ms (intra 21, inter 19) | 18.6–18.7 fps (19.2 with `-asyncclear`), GPU-bound at ~51 ms |

- **CDEF v2** is 7–9x faster than upstream and bit-exact. It is the default (`-cdefv1`
  selects upstream's shader).
- **Intra v2** (`-intrav2`) is bit-exact but 2.3–2.4x slower. It is a negative result, off by
  default and kept for reference.
- **Intra optimisation A/B** (`-intraopt`): upstream builds its `intra_main` shader with FXC
  optimisation disabled. With optimisation on it is no faster and decodes wrongly (a
  timing-dependent race), which is probably why upstream turned it off. Negative result.
- **A third frame in flight** (`-slots 3`) lets CPU and GPU overlap: the decode thread's wait
  for a free slot drops from 5.6–8.3 ms to 0.3–0.6 ms per frame. It only fits in memory with
  `-lowmem`. All combinations are md5-exact. On the SVT clip the CPU (entropy decoding,
  ~34 ms on one core) is now the limit; on the Jellyfin clip the GPU is.
- **Memory**: a console app may use 1 GiB. At 4K the decoder took 882.7 MiB of it (app memory
  after creation, 2 slots), then 812.7 MiB, and 765.5 MiB sized for the stream (measured).
  The default now also shares one back buffer, 24 MiB less (computed: 615.7 MiB of D3D12
  buffers for a 4K 10-bit stream). With `-lowmem` it is 663.4 MiB and with
  `-lowmem -slots 3` 773.5 MiB (measured). A decoder that
  does not fit is refused instead of losing the D3D12 device, and a destroyed decoder returns
  its memory (what stays charged to the app afterwards, up to ~250 MiB, is reused by the next decoder, and the
  player's guard counts it as available).
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
  host checks keep it that way (`testing/sync_check`). On WARP all 251 conformance streams
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
  rewritten so that FXC does not emit the pair, the workflow scans the listings of all shaders for
  such pairs (`testing/dxbc_scan`), and the output on WARP is unchanged
  ([gpu-av1-decoder.md](docs/gpu-av1-decoder.md#a-paired-read-before-an-overlapping-in-place-write-2026-11-12)).
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
- **Every stream we test is md5-identical to dav1d on the console and on WARP**: 251 conformance
  streams on WARP; on the console the 8-bit and 10-bit clips from 66x66 to 4K, hidden frames, film
  grain, global motion, spatial layers, size changes, scaled references and superres.
  The 10-bit stream with scaled references that stood here as wrong on the console is right: its md5
  references had been made with ffmpeg scaling every frame to the size of the first.

Next targets: the Rust host ([docs/rust-port.md](docs/rust-port.md), started 2026-11-12; steps 1 to 4 of
5 are in: a Cargo workspace that builds the decoder, `rav1g-dec`, the conformance tool in Rust,
bindings with layout tests for the structs shared with the shaders and with the entropy decoder, and the
whole host in Rust, from the creation of a decoder to the completion thread; the cargo build compiles no
C++ of the decoder. Step 5 is the Rust API and the C ABI the test player links), then
inter and intra prediction on the GPU (a measured `-interfast` prototype showed that inter time is per
block and per dispatch, not filtering; see
[gpu-av1-decoder.md](docs/gpu-av1-decoder.md)) and entropy decoding on the CPU (frame-parallel or a
faster parser). The goals, in order: heavy 4K at 30 fps in real time (at most 33.3 ms per frame; not met
yet, nor the 41.7 ms of 24 fps), then faster still, then decoding with as much of the CPU, GPU and
memory as possible left to the application.

## Documentation

- [docs/test-player.md](docs/test-player.md): building, installing and running the test
  player, its defaults and flags, log lines, md5 checks (references can be made with dav1d:
  `testing/md5ref/make_md5_ref.sh`) and the host-side shader checks.
- [docs/gpu-av1-decoder.md](docs/gpu-av1-decoder.md): history, measurements and reasoning.
- [docs/warp-decoder.md](docs/warp-decoder.md): `warp_decoder` and `rav1g-dec`, the decoder as a
  console program on the WARP software adapter, and the conformance run of every push.
- [docs/rust-port.md](docs/rust-port.md): the Rust port, its steps, gates and progress. The workspace
  builds with `RAV1G_SHADER_DIR=<shader headers> cargo build --release` on Windows and with
  `cargo xwin build --target x86_64-pc-windows-msvc` on Linux.

## Licence

BSD-2-Clause (`LICENSE.txt`, `libav1/LICENSE`) plus the Alliance for Open Media Patent
License 1.0 (`libav1/PATENTS`). `third_party/libwebm` carries its own licence and patent
notices (`third_party/libwebm/LICENSE.TXT`, `third_party/libwebm/PATENTS.TXT`). Keep all of
them when redistributing source or binaries; CI artifacts include them under `licenses/`.

## Disclaimers

Not affiliated with or endorsed by Microsoft. Xbox is a trademark of Microsoft Corporation.

Independent project, not affiliated with rav1e or rav1d.

Not affiliated with or endorsed by the Alliance for Open Media.
